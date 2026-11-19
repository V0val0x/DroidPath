use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use droidpath_core::{
    DroidPathAdbRunner, DroidPathHttpServer, DroidPathManager, DroidPathMcpServerCore,
    DroidPathPersistenceCoordinator, DroidPathStdioTransport, ScrcpyBridge,
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
    Help,
    Version,
}

struct CliConfig {
    mode: CommandMode,
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

OPTIONS:
  --stdio                 Run JSON-RPC 2.0 MCP server over stdin/stdout (default)
  --http                  Run loopback HTTP/SSE MCP server on 127.0.0.1:<port>
  --port <port>           HTTP/SSE listen port (default: 8766)
  --daemon-port <port>    Android Shadow DOM daemon TCP forward port (default: 27189)
  --serial <serial>       Target ADB device serial (for deploy / stop-daemon)
  --graph-path <path>     Spatial Graph JSON persistence path (default: ~/.droidpath/shadow_graph.json)
  --daemon-jar <path>     Path to single-DEX android-daemon.jar
  --auth-token <token>    Optional Bearer token required on HTTP/SSE requests
  -v, --version           Print version
  -h, --help              Show this help
"#,
        DroidPathMcpServerCore::SERVER_VERSION
    )
}

fn parse_args(args: &[String]) -> Result<CliConfig, String> {
    let mut cfg = CliConfig {
        mode: CommandMode::Stdio,
        port: 8766,
        daemon_port: 27189,
        graph_path: DroidPathPersistenceCoordinator::default_storage_path(),
        daemon_jar_path: None,
        auth_token: None,
    };

    let mut serial: Option<String> = None;
    let mut subcommand: Option<&str> = None;
    let mut i = 1;

    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "deploy" | "stop-daemon" | "devices" | "stats" | "doctor" if subcommand.is_none() => {
                subcommand = Some(arg);
            }
            "--stdio" => cfg.mode = CommandMode::Stdio,
            "--http" => cfg.mode = CommandMode::Http,
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
                if p == 0 {
                    return Err("--daemon-port must be > 0".to_string());
                }
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
            _ => unreachable!(),
        };
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cfg = match parse_args(&args) {
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
        _ => {}
    }

    let adb = DroidPathAdbRunner::default();
    let manager = DroidPathManager::new(
        cfg.daemon_port,
        cfg.graph_path.clone(),
        cfg.daemon_jar_path.clone(),
        adb.clone(),
        None,
        None,
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
            if let Err(err) = http.start(cfg.port, cfg.auth_token).await {
                DroidPathManager::log_stderr(&format!("Failed to start HTTP server: {err}"));
                return ExitCode::FAILURE;
            }
            let _ = tokio::signal::ctrl_c().await;
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

        CommandMode::Help | CommandMode::Version => unreachable!(),
    }
}
