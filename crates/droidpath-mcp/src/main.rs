use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use droidpath_core::{
    DroidPathAdbRunner, DroidPathHttpServer, DroidPathManager, DroidPathMcpServerCore,
    DroidPathPersistenceCoordinator, DroidPathStdioTransport, ScrcpyBridge, ToolSchemaExporter,
    ToolSchemaFormat,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Verifies that an actual NDJSON `android-daemon` process is alive behind `127.0.0.1:<port>`,
/// rather than a stale `adb forward` tunnel that accepts TCP and immediately sends EOF.
async fn probe_daemon_alive(port: u16) -> bool {
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        std::time::Duration::from_millis(350),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    else {
        return false;
    };
    let _ = stream.write_all(b"{\"cmd\":\"ping\"}\n").await;
    let _ = stream.flush().await;
    let mut buf = [0u8; 64];
    matches!(
        tokio::time::timeout(std::time::Duration::from_millis(350), stream.read(&mut buf)).await,
        Ok(Ok(n)) if n > 0 && buf[..n].contains(&b'{')
    )
}

#[derive(Debug)]
enum CommandMode {
    Stdio,
    Http,
    Deploy { serial: Option<String> },
    StopDaemon { serial: Option<String> },
    Devices,
    Stats,
    Doctor,
    Schemas { format: ToolSchemaFormat },
    Help,
    Version,
}

struct CliConfig {
    mode: CommandMode,
    bind_host: String,
    port: u16,
    daemon_port: u16,
    graph_path: PathBuf,
    daemon_jar_path: Option<PathBuf>,
    auth_token: Option<String>,
}

fn usage_text() -> String {
    format!(
        r#"droidpath-mcp v{} — Cross-Platform Persistent A* Symbolic UI Router & Shadow DOM MCP Server

USAGE:
  droidpath-mcp [--stdio | --http] [OPTIONS]
  droidpath-mcp <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS:
  deploy                  Auto-inject android-daemon.jar (SHA-256 cached adb push + forward + app_process)
  stop-daemon             Stop running android-daemon on device and remove adb port forward
  devices                 List connected USB & Wireless ADB Android devices (JSON)
  stats                   Show persisted Spatial Graph telemetry (JSON)
  doctor                  Run environment & device diagnostics (ADB, daemon JAR, TCP :27189, scrcpy)
  schemas                 Export standardized JSON tool schemas (OpenAI, Anthropic, MCP, or all)

OPTIONS:
  --stdio                 Run JSON-RPC 2.0 MCP server over stdin/stdout (default)
  --http                  Run HTTP/SSE MCP server (default bind: 127.0.0.1:<port>)
  --bind-host <addr>      HTTP/SSE bind address; non-loopback requires --auth-token (default: 127.0.0.1, or DROIDPATH_BIND_HOST)
  --port <port>           HTTP/SSE listen port (default: 8766)
  --daemon-port <port>    Android Shadow DOM daemon TCP forward port; 0 = dynamic ephemeral port (default: 27189)
  --format <fmt>          Schema export format for 'schemas': openai | anthropic | mcp | all (default: all)
  --export-schemas <fmt>  Shorthand to export JSON tool schemas and exit (openai | anthropic | mcp | all)
  --serial <serial>       Target ADB device serial (for deploy / stop-daemon)
  --graph-path <path>     Spatial Graph JSON persistence path (default: ~/.droidpath/shadow_graph.json)
  --daemon-jar <path>     Path to single-DEX android-daemon.jar
  --auth-token <token>    Optional Bearer token for HTTP/SSE requests (mandatory for non-loopback --bind-host; or DROIDPATH_AUTH_TOKEN)
  -v, --version           Print version
  -h, --help              Show this help
"#,
        DroidPathMcpServerCore::SERVER_VERSION
    )
}

fn parse_args(args: &[String]) -> Result<CliConfig, String> {
    let env_bind_host = std::env::var("DROIDPATH_BIND_HOST").ok();
    let env_auth_token = std::env::var("DROIDPATH_AUTH_TOKEN").ok();
    parse_args_with_env(args, env_bind_host.as_deref(), env_auth_token.as_deref())
}

fn parse_args_with_env(
    args: &[String],
    env_bind_host: Option<&str>,
    env_auth_token: Option<&str>,
) -> Result<CliConfig, String> {
    let default_bind_host = env_bind_host
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("127.0.0.1")
        .to_string();
    let default_auth_token = env_auth_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let mut cfg = CliConfig {
        mode: CommandMode::Stdio,
        bind_host: default_bind_host,
        port: 8766,
        daemon_port: 27189,
        graph_path: DroidPathPersistenceCoordinator::default_storage_path(),
        daemon_jar_path: None,
        auth_token: default_auth_token,
    };

    let mut serial: Option<String> = None;
    let mut subcommand: Option<&str> = None;
    let mut schema_format = ToolSchemaFormat::All;
    let mut i = 1;

    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "deploy" | "stop-daemon" | "devices" | "stats" | "doctor" | "schemas"
                if subcommand.is_none() =>
            {
                subcommand = Some(arg);
            }
            "--stdio" => cfg.mode = CommandMode::Stdio,
            "--http" => cfg.mode = CommandMode::Http,
            "--bind-host" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --bind-host".to_string())?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err("--bind-host must not be empty".to_string());
                }
                cfg.bind_host = trimmed.to_string();
                i += 1;
            }
            "--format" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --format".to_string())?;
                schema_format = ToolSchemaFormat::parse(raw).ok_or_else(|| {
                    format!("Invalid --format '{raw}' (expected: openai | anthropic | mcp | all)")
                })?;
                i += 1;
            }
            "--export-schemas" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --export-schemas".to_string())?;
                schema_format = ToolSchemaFormat::parse(raw).ok_or_else(|| {
                    format!("Invalid --export-schemas '{raw}' (expected: openai | anthropic | mcp | all)")
                })?;
                cfg.mode = CommandMode::Schemas {
                    format: schema_format,
                };
                return Ok(cfg);
            }
            "--port" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --port".to_string())?;
                let p: u16 = raw
                    .parse()
                    .map_err(|_| format!("Invalid value for --port: '{raw}'"))?;
                if p == 0 {
                    return Err("--port must be > 0".to_string());
                }
                cfg.port = p;
                i += 1;
            }
            "--daemon-port" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --daemon-port".to_string())?;
                let p: u16 = raw
                    .parse()
                    .map_err(|_| format!("Invalid value for --daemon-port: '{raw}'"))?;
                cfg.daemon_port = p;
                i += 1;
            }
            "--serial" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --serial".to_string())?;
                serial = Some(raw.clone());
                i += 1;
            }
            "--graph-path" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --graph-path".to_string())?;
                cfg.graph_path = PathBuf::from(raw);
                i += 1;
            }
            "--daemon-jar" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --daemon-jar".to_string())?;
                cfg.daemon_jar_path = Some(PathBuf::from(raw));
                i += 1;
            }
            "--auth-token" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "Missing value for --auth-token".to_string())?;
                cfg.auth_token = Some(raw.clone());
                i += 1;
            }
            "-h" | "--help" => {
                cfg.mode = CommandMode::Help;
                return Ok(cfg);
            }
            "-v" | "--version" => {
                cfg.mode = CommandMode::Version;
                return Ok(cfg);
            }
            other => {
                return Err(format!("Unknown argument: '{other}'"));
            }
        }
        i += 1;
    }

    if let Some(sub) = subcommand {
        cfg.mode = match sub {
            "deploy" => CommandMode::Deploy { serial },
            "stop-daemon" => CommandMode::StopDaemon { serial },
            "devices" => CommandMode::Devices,
            "stats" => CommandMode::Stats,
            "doctor" => CommandMode::Doctor,
            "schemas" => CommandMode::Schemas {
                format: schema_format,
            },
            _ => unreachable!(),
        };
    }

    Ok(cfg)
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut cfg = match parse_args(&args) {
        Ok(c) => c,
        Err(err) => {
            DroidPathManager::log_stderr(&err);
            return ExitCode::from(2);
        }
    };

    match cfg.mode {
        CommandMode::Version => {
            println!("droidpath-mcp {}", DroidPathMcpServerCore::SERVER_VERSION);
            return ExitCode::SUCCESS;
        }
        CommandMode::Help => {
            print!("{}", usage_text());
            return ExitCode::SUCCESS;
        }
        CommandMode::Schemas { format } => {
            let exported = ToolSchemaExporter::export(format);
            println!(
                "{}",
                serde_json::to_string_pretty(&exported).unwrap_or_else(|_| "{}".to_string())
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    if cfg.daemon_port == 0 {
        match DroidPathAdbRunner::allocate_ephemeral_port() {
            Ok(p) => {
                cfg.daemon_port = p;
                DroidPathManager::log_stderr(&format!(
                    "Allocated ephemeral daemon port 127.0.0.1:{p}"
                ));
            }
            Err(err) => {
                DroidPathManager::log_stderr(&format!(
                    "Failed to allocate ephemeral daemon port: {err}"
                ));
                return ExitCode::FAILURE;
            }
        }
    }

    let adb = DroidPathAdbRunner::default();
    let manager = DroidPathManager::new_with_seed(
        cfg.daemon_port,
        cfg.graph_path.clone(),
        cfg.daemon_jar_path.clone(),
        adb.clone(),
        None,
        None,
        true,
    );

    match cfg.mode {
        CommandMode::Deploy { serial } => {
            let target_serial = match adb.resolve_target_serial(serial.as_deref()).await {
                Ok(s) => s,
                Err(e) => {
                    DroidPathManager::log_stderr(&e.to_string());
                    return ExitCode::FAILURE;
                }
            };
            match manager
                .deploy_and_launch_daemon(&target_serial, cfg.daemon_port)
                .await
            {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    DroidPathManager::log_stderr(&e);
                    ExitCode::FAILURE
                }
            }
        }

        CommandMode::StopDaemon { serial } => {
            let target_serial = match adb.resolve_target_serial(serial.as_deref()).await {
                Ok(s) => s,
                Err(e) => {
                    DroidPathManager::log_stderr(&e.to_string());
                    return ExitCode::FAILURE;
                }
            };
            let _ = manager
                .stop_remote_daemon(&target_serial, cfg.daemon_port)
                .await;
            println!(
                "{}",
                json!({ "stopped": true, "serial": target_serial, "port": cfg.daemon_port })
            );
            ExitCode::SUCCESS
        }

        CommandMode::Devices => match adb.list_devices().await {
            Ok(devices) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&devices).unwrap_or_else(|_| "[]".to_string())
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                DroidPathManager::log_stderr(&e.to_string());
                ExitCode::FAILURE
            }
        },

        CommandMode::Stats => {
            let stats = manager.graph_stats().await;
            let payload = json!({
                "vertex_count": stats.vertex_count,
                "seed_vertex_count": stats.seed_vertex_count,
                "hydrated_vertex_count": stats.hydrated_vertex_count,
                "edge_count": stats.edge_count,
                "current_state_hash": stats.current_state_hash.map(|h| format!("0x{h:016x}")),
                "current_package": stats.current_package,
                "connection_state": stats.connection_state,
                "has_fresh_shadow_dom": stats.has_fresh_shadow_dom,
                "storage_path": stats.storage_path,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
            );
            ExitCode::SUCCESS
        }

        CommandMode::Doctor => {
            let adb_path = DroidPathAdbRunner::resolve_adb_path();
            let devices = adb.list_devices().await.ok().unwrap_or_default();
            let daemon_jar = manager.resolve_daemon_jar_path();
            let scrcpy_server = ScrcpyBridge::resolve_scrcpy_server_path();
            let stats = manager.graph_stats().await;
            let tcp_reachable = probe_daemon_alive(cfg.daemon_port).await;

            let report = json!({
                "version": DroidPathMcpServerCore::SERVER_VERSION,
                "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                "adb": {
                    "found": adb_path.is_some(),
                    "path": adb_path.map(|p| p.to_string_lossy().into_owned()),
                },
                "devices": devices,
                "daemon_jar": {
                    "found": daemon_jar.is_some(),
                    "path": daemon_jar.map(|p| p.to_string_lossy().into_owned()),
                },
                "daemon_tcp": {
                    "port": cfg.daemon_port,
                    "reachable": tcp_reachable,
                },
                "scrcpy_server": {
                    "found": scrcpy_server.is_some(),
                    "path": scrcpy_server.map(|p| p.to_string_lossy().into_owned()),
                },
                "graph": {
                    "path": stats.storage_path,
                    "exists": cfg.graph_path.is_file(),
                    "vertex_count": stats.vertex_count,
                    "seed_vertex_count": stats.seed_vertex_count,
                    "hydrated_vertex_count": stats.hydrated_vertex_count,
                    "edge_count": stats.edge_count,
                }
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
            );
            ExitCode::SUCCESS
        }

        CommandMode::Http => {
            manager.load_persisted_graph_if_needed().await;
            let server_core = Arc::new(DroidPathMcpServerCore::new(Arc::clone(&manager)));
            let http = DroidPathHttpServer::new(server_core);
            if let Err(err) = http.start_on(&cfg.bind_host, cfg.port, cfg.auth_token).await {
                DroidPathManager::log_stderr(&format!("Failed to start HTTP server: {err}"));
                return ExitCode::FAILURE;
            }
            wait_for_shutdown_signal().await;
            http.stop().await;
            let _ = tokio::time::timeout(std::time::Duration::from_millis(2500), manager.stop())
                .await;
            ExitCode::SUCCESS
        }

        CommandMode::Stdio => {
            let server_core = Arc::new(DroidPathMcpServerCore::new(manager));
            if let Err(err) = DroidPathStdioTransport::run(server_core).await {
                DroidPathManager::log_stderr(&format!("Stdio transport error: {err}"));
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }

        CommandMode::Help | CommandMode::Version | CommandMode::Schemas { .. } => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn default_bind_host_is_loopback() {
        let cfg = parse_args_with_env(&vec_args(&["droidpath-mcp", "--http"]), None, None).unwrap();
        assert_eq!(cfg.bind_host, "127.0.0.1");
        assert!(cfg.auth_token.is_none());
        assert!(matches!(cfg.mode, CommandMode::Http));
    }

    #[test]
    fn env_bind_host_and_auth_token_are_honored_and_overridden_by_cli_flags() {
        let from_env = parse_args_with_env(
            &vec_args(&["droidpath-mcp", "--http"]),
            Some("0.0.0.0"),
            Some("env-secret"),
        )
        .unwrap();
        assert_eq!(from_env.bind_host, "0.0.0.0");
        assert_eq!(from_env.auth_token.as_deref(), Some("env-secret"));

        let blank_env = parse_args_with_env(
            &vec_args(&["droidpath-mcp", "--http"]),
            Some("   "),
            Some("   "),
        )
        .unwrap();
        assert_eq!(blank_env.bind_host, "127.0.0.1");
        assert!(blank_env.auth_token.is_none());

        let overridden = parse_args_with_env(
            &vec_args(&[
                "droidpath-mcp",
                "--http",
                "--bind-host",
                "127.0.0.1",
                "--auth-token",
                "cli-secret",
            ]),
            Some("0.0.0.0"),
            Some("env-secret"),
        )
        .unwrap();
        assert_eq!(overridden.bind_host, "127.0.0.1");
        assert_eq!(overridden.auth_token.as_deref(), Some("cli-secret"));
    }

    #[test]
    fn empty_bind_host_is_rejected() {
        let err = parse_args_with_env(
            &vec_args(&["droidpath-mcp", "--http", "--bind-host", "   "]),
            None,
            None,
        )
        .err()
        .unwrap();
        assert!(err.contains("--bind-host"));
    }
}
