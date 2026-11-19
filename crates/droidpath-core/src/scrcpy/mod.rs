use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use flate2::read::DeflateDecoder;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;

use crate::adb::DroidPathAdbRunner;

/// Binary WebSocket frame types sent to WebCodecs frontend:
/// - `0x01`: Metadata `[0x01, width: u32 BE, height: u32 BE]`
/// - `0x02`: H.264 Annex-B video packet `[0x02, flags: u8 (bit0=config, bit1=keyframe), pts_us: u64 BE, payload...]`
pub const WS_FRAME_META: u8 = 0x01;
pub const WS_FRAME_VIDEO: u8 = 0x02;
pub const VIDEO_FLAG_CONFIG: u8 = 1 << 0;
pub const VIDEO_FLAG_KEYFRAME: u8 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrcpyMetaOption {
    /// scrcpy 4.x / 5.x (`send_stream_meta=false`)
    SendStreamMeta,
    /// scrcpy 2.x / 3.x (`send_codec_meta=false`)
    SendCodecMeta,
    /// scrcpy 1.x or unknown (omit flag)
    None,
}

impl ScrcpyMetaOption {
    pub fn disable_arg(self) -> Option<&'static str> {
        match self {
            Self::SendStreamMeta => Some("send_stream_meta=false"),
            Self::SendCodecMeta => Some("send_codec_meta=false"),
            Self::None => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrcpyJarInspection {
    pub version: Option<String>,
    pub meta_option: ScrcpyMetaOption,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrcpySessionInfo {
    pub active: bool,
    pub serial: String,
    pub ws_url: String,
    pub ws_token: String,
    pub video_width: u32,
    pub video_height: u32,
    pub scrcpy_version: String,
}

#[derive(Debug, Clone)]
pub struct ScrcpyStartConfig {
    pub max_size: u32,
    pub max_fps: u32,
    pub bit_rate: u32,
    pub stay_awake: bool,
    pub turn_screen_off: bool,
}

impl Default for ScrcpyStartConfig {
    fn default() -> Self {
        Self {
            max_size: 1280,
            max_fps: 60,
            bit_rate: 8_000_000,
            stay_awake: true,
            turn_screen_off: false,
        }
    }
}

struct ActiveSession {
    serial: String,
    local_port: u16,
    ws_url: String,
    ws_token: String,
    scrcpy_version: String,
    server_child: Option<Child>,
    control_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    tasks: Vec<JoinHandle<()>>,
    did_turn_screen_off: bool,
}

/// Cross-platform `scrcpy-server` H.264 -> loopback WebSocket bridge for WebCodecs decoding
/// plus binary scrcpy control protocol injection (touch, scroll, keycode, text, power).
pub struct ScrcpyBridge {
    adb_runner: DroidPathAdbRunner,
    session: Mutex<Option<ActiveSession>>,
    dimensions: Arc<RwLock<(u32, u32)>>,
    latest_config_frame: Arc<RwLock<Option<Vec<u8>>>>,
    latest_keyframe: Arc<RwLock<Option<Vec<u8>>>>,
    video_tx: broadcast::Sender<Vec<u8>>,
    clipboard_seq: Mutex<u64>,
}

impl ScrcpyBridge {
    pub fn new(adb_runner: DroidPathAdbRunner) -> Self {
        let (video_tx, _) = broadcast::channel(120);
        Self {
            adb_runner,
            session: Mutex::new(None),
            dimensions: Arc::new(RwLock::new((1080, 2400))),
            latest_config_frame: Arc::new(RwLock::new(None)),
            latest_keyframe: Arc::new(RwLock::new(None)),
            video_tx,
            clipboard_seq: Mutex::new(0),
        }
    }

    pub fn resolve_scrcpy_server_path() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(explicit) = std::env::var("DROIDPATH_SCRCPY_SERVER") {
            if !explicit.is_empty() {
                candidates.push(PathBuf::from(explicit));
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("scrcpy-server"));
                candidates.push(dir.join("resources").join("scrcpy-server"));
                candidates.push(dir.join("../Resources/bin/scrcpy-server"));
                candidates.push(dir.join("../share/droidpath/scrcpy-server"));
            }
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".droidpath").join("share").join("scrcpy-server"));
        }

        if cfg!(windows) {
            candidates.push(PathBuf::from(r"C:\Program Files\scrcpy\scrcpy-server"));
            if let Some(home) = dirs::home_dir() {
                candidates.push(
                    home.join("scoop")
                        .join("apps")
                        .join("scrcpy")
                        .join("current")
                        .join("scrcpy-server"),
                );
            }
            if let Some(path_os) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path_os) {
                    candidates.push(dir.join("scrcpy-server"));
                }
            }
        } else if cfg!(target_os = "macos") {
            candidates.push(PathBuf::from("/opt/homebrew/share/scrcpy/scrcpy-server"));
            candidates.push(PathBuf::from("/usr/local/share/scrcpy/scrcpy-server"));
            if let Ok(entries) = std::fs::read_dir("/opt/homebrew/Cellar/scrcpy") {
                for entry in entries.flatten() {
                    candidates.push(entry.path().join("share/scrcpy/scrcpy-server"));
                }
            }
        } else {
            candidates.push(PathBuf::from("/usr/share/scrcpy/scrcpy-server"));
            candidates.push(PathBuf::from("/usr/local/share/scrcpy/scrcpy-server"));
        }

        candidates.into_iter().find(|p| p.is_file())
    }

    /// Inspects `classes.dex` inside a `scrcpy-server` ZIP/JAR archive to accurately extract:
    /// 1. The exact server version (even when `scrcpy` CLI is not on PATH), and
    /// 2. Whether the JAR expects `send_stream_meta` (v4+/v5+) or `send_codec_meta` (v2/v3).
    pub fn inspect_scrcpy_server_jar(jar_bytes: &[u8]) -> ScrcpyJarInspection {
        let Some(dex_bytes) = extract_zip_entry(jar_bytes, "classes.dex", 32 * 1024 * 1024) else {
            return ScrcpyJarInspection {
                version: None,
                meta_option: ScrcpyMetaOption::None,
            };
        };

        let meta_option = if find_subslice(&dex_bytes, b"send_stream_meta").is_some() {
            ScrcpyMetaOption::SendStreamMeta
        } else if find_subslice(&dex_bytes, b"send_codec_meta").is_some() {
            ScrcpyMetaOption::SendCodecMeta
        } else {
            ScrcpyMetaOption::None
        };

        // Every Genymobile scrcpy-server DEX embeds:
        // "The server version (<VERSION>) does not match the client ("
        let prefix = b"The server version (";
        let suffix = b") does not match the client (";
        let version = find_subslice(&dex_bytes, prefix).and_then(|start| {
            let v_start = start + prefix.len();
            let search_end = (v_start + 32 + suffix.len()).min(dex_bytes.len());
            let rel_end = find_subslice(&dex_bytes[v_start..search_end], suffix)?;
            let raw = std::str::from_utf8(&dex_bytes[v_start..v_start + rel_end]).ok()?;
            let trimmed = raw.trim();
            if !trimmed.is_empty()
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            {
                Some(trimmed.to_string())
            } else {
                None
            }
        });

        ScrcpyJarInspection {
            version,
            meta_option,
        }
    }

    pub fn detect_scrcpy_version(server_path: &PathBuf) -> String {
        if let Ok(v) = std::env::var("DROIDPATH_SCRCPY_VERSION") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        if let Ok(jar_bytes) = std::fs::read(server_path) {
            if let Some(v) = Self::inspect_scrcpy_server_jar(&jar_bytes).version {
                return v;
            }
        }

        // Check resolved symlink / parent path for Homebrew Cellar version (e.g. .../scrcpy/5.0.1/...)
        let canonical = std::fs::canonicalize(server_path).unwrap_or_else(|_| server_path.clone());
        let parts: Vec<String> = canonical
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        for win in parts.windows(2) {
            if win[0] == "scrcpy"
                && win[1]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            {
                return win[1].clone();
            }
        }

        // Try invoking `scrcpy --version` if on PATH
        let scrcpy_bin = if cfg!(windows) { "scrcpy.exe" } else { "scrcpy" };
        if let Ok(out) = std::process::Command::new(scrcpy_bin)
            .arg("--version")
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(first_line) = text.lines().next() {
                    let tokens: Vec<&str> = first_line.split_whitespace().collect();
                    if tokens.len() >= 2 && tokens[0].eq_ignore_ascii_case("scrcpy") {
                        return tokens[1].trim_start_matches('v').to_string();
                    }
                }
            }
        }

        "3.1".to_string()
    }

    pub async fn dimensions(&self) -> (u32, u32) {
        *self.dimensions.read().await
    }

    pub async fn session_info(&self) -> Option<ScrcpySessionInfo> {
        let guard = self.session.lock().await;
        let s = guard.as_ref()?;
        let (w, h) = *self.dimensions.read().await;
        Some(ScrcpySessionInfo {
            active: true,
            serial: s.serial.clone(),
            ws_url: s.ws_url.clone(),
            ws_token: s.ws_token.clone(),
            video_width: w,
            video_height: h,
            scrcpy_version: s.scrcpy_version.clone(),
        })
    }

    pub async fn start(
        &self,
        serial: &str,
        config: ScrcpyStartConfig,
    ) -> Result<ScrcpySessionInfo, String> {
        if !DroidPathAdbRunner::validate_serial(serial) {
            return Err(format!("Invalid ADB device serial '{serial}'."));
        }
        self.stop().await;

        let server_path = Self::resolve_scrcpy_server_path()
            .ok_or_else(|| "scrcpy-server binary not found. Install scrcpy or set DROIDPATH_SCRCPY_SERVER.".to_string())?;

        // Wake device screen
        let _ = self
            .adb_runner
            .execute(
                &["-s", serial, "shell", "input", "keyevent", "KEYCODE_WAKEUP"],
                2.0,
            )
            .await;

        // Read JAR once for both DEX inspection (exact version + meta flag) and SHA-256 push check
        let remote_jar = "/data/local/tmp/scrcpy-server-droidpath.jar";
        let jar_bytes = tokio::fs::read(&server_path)
            .await
            .map_err(|e| format!("Failed to read scrcpy-server: {e}"))?;
        let inspection = Self::inspect_scrcpy_server_jar(&jar_bytes);
        let version = std::env::var("DROIDPATH_SCRCPY_VERSION")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or(inspection.version)
            .unwrap_or_else(|| Self::detect_scrcpy_version(&server_path));

        let meta_option = if inspection.meta_option != ScrcpyMetaOption::None {
            inspection.meta_option
        } else {
            let major: u32 = version
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3);
            if major >= 4 {
                ScrcpyMetaOption::SendStreamMeta
            } else if major >= 2 {
                ScrcpyMetaOption::SendCodecMeta
            } else {
                ScrcpyMetaOption::None
            }
        };

        let local_sha: String = Sha256::digest(&jar_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sha_cmd = format!(
            "sha256sum '{remote_jar}' 2>/dev/null || toybox sha256sum '{remote_jar}' 2>/dev/null"
        );
        let remote_sha_raw = self
            .adb_runner
            .execute(&["-s", serial, "shell", &sha_cmd], 3.0)
            .await
            .unwrap_or_default();
        if remote_sha_raw.split_whitespace().next().unwrap_or("") != local_sha {
            let p_str = server_path.to_string_lossy();
            self.adb_runner
                .execute(&["-s", serial, "push", &p_str, remote_jar], 10.0)
                .await
                .map_err(|e| format!("Failed to push scrcpy-server: {e}"))?;
        }

        let scid = format!(
            "{:08x}",
            rand::rng().random_range(0x1000_0000u32..=0x7fff_ffffu32)
        );
        let local_port = rand::rng().random_range(27100u16..=27900u16);
        let tcp_spec = format!("tcp:{local_port}");
        let abstract_spec = format!("localabstract:scrcpy_{scid}");
        let _ = self
            .adb_runner
            .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
            .await;
        self.adb_runner
            .execute(&["-s", serial, "forward", &tcp_spec, &abstract_spec], 3.0)
            .await
            .map_err(|e| format!("adb forward for scrcpy failed: {e}"))?;

        let adb_path = match self.adb_runner.adb_binary_path().await {
            Ok(p) => p,
            Err(e) => {
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(e.to_string());
            }
        };

        let classpath_arg = format!("CLASSPATH={remote_jar}");
        let scid_arg = format!("scid={scid}");
        let max_size_arg = format!("max_size={}", config.max_size);
        let max_fps_arg = format!("max_fps={}", config.max_fps);
        let bit_rate_arg = format!("video_bit_rate={}", config.bit_rate);
        let stay_awake_arg = format!(
            "stay_awake={}",
            if config.stay_awake { "true" } else { "false" }
        );

        let mut server_args: Vec<&str> = vec![
            "-s",
            serial,
            "shell",
            &classpath_arg,
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            &version,
            &scid_arg,
            "log_level=info",
            "tunnel_forward=true",
            "audio=false",
            "control=true",
            "video=true",
            "clipboard_autosync=false",
            "send_device_meta=false",
            "send_dummy_byte=true",
            "send_frame_meta=true",
            &max_size_arg,
            &max_fps_arg,
            &bit_rate_arg,
            "video_codec=h264",
            &stay_awake_arg,
        ];
        // Disable codec/stream meta headers so ALL scrcpy versions (2.x/3.x/4.x/5.x) emit identical
        // 12-byte framed Annex-B H.264 packets immediately after the dummy byte.
        if let Some(flag) = meta_option.disable_arg() {
            server_args.push(flag);
        }

        let mut cmd = Command::new(&adb_path);
        cmd.args(&server_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(format!("Failed to spawn scrcpy-server: {e}"));
            }
        };

        tokio::time::sleep(Duration::from_millis(220)).await;

        // Connect video socket (verifying 1 dummy byte) then control socket
        let mut video_stream = match Self::connect_video_socket(local_port).await {
            Ok(s) => s,
            Err(err) => {
                let _ = child.kill().await;
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(err);
            }
        };

        let control_stream = match Self::connect_control_socket(local_port).await {
            Ok(s) => s,
            Err(err) => {
                let _ = child.kill().await;
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(err);
            }
        };

        // Bind loopback WebSocket server for WebCodecs binary video delivery
        let ws_listener = match TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(e) => {
                let _ = child.kill().await;
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(format!("Failed to bind video WebSocket: {e}"));
            }
        };
        let ws_port = match ws_listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                let _ = child.kill().await;
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "forward", "--remove", &tcp_spec], 2.0)
                    .await;
                return Err(e.to_string());
            }
        };
        let ws_token = uuid::Uuid::new_v4().simple().to_string();
        let ws_url = format!("ws://127.0.0.1:{ws_port}/?token={ws_token}");

        *self.latest_config_frame.write().await = None;
        *self.latest_keyframe.write().await = None;

        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(128);
        let (mut ctrl_reader, mut ctrl_writer) = control_stream.into_split();

        let ctrl_write_task = tokio::spawn(async move {
            while let Some(msg) = control_rx.recv().await {
                if ctrl_writer.write_all(&msg).await.is_err() || ctrl_writer.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let ctrl_read_task = tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            loop {
                match ctrl_reader.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let (initial_dims_tx, initial_dims_rx) = oneshot::channel::<(u32, u32)>();
        let mut initial_dims_tx = Some(initial_dims_tx);

        let video_tx = self.video_tx.clone();
        let dims_ref = Arc::clone(&self.dimensions);
        let latest_cfg = Arc::clone(&self.latest_config_frame);
        let latest_key = Arc::clone(&self.latest_keyframe);
        let video_read_task = tokio::spawn(async move {
            let mut hdr = [0u8; 12];
            loop {
                if video_stream.read_exact(&mut hdr).await.is_err() {
                    break;
                }
                let pts_and_flags = u64::from_be_bytes([
                    hdr[0], hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6], hdr[7],
                ]);
                let packet_size = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
                if packet_size == 0 || packet_size > 16 * 1024 * 1024 {
                    break;
                }
                let is_config = (pts_and_flags & (1u64 << 63)) != 0;
                let is_keyframe = (pts_and_flags & (1u64 << 62)) != 0;
                let pts_us = pts_and_flags & 0x3FFF_FFFF_FFFF_FFFF;

                let mut frame = Vec::with_capacity(10 + packet_size);
                frame.push(WS_FRAME_VIDEO);
                let mut flags = 0u8;
                if is_config {
                    flags |= VIDEO_FLAG_CONFIG;
                }
                if is_keyframe {
                    flags |= VIDEO_FLAG_KEYFRAME;
                }
                frame.push(flags);
                frame.extend_from_slice(&pts_us.to_be_bytes());
                frame.resize(10 + packet_size, 0);
                if video_stream.read_exact(&mut frame[10..]).await.is_err() {
                    break;
                }

                // Parse H.264 SPS dimensions on initial config AND dynamically on screen rotation
                if let Some((w, h)) = parse_h264_sps_dimensions(&frame[10..]) {
                    let prev = *dims_ref.read().await;
                    if prev != (w, h) {
                        *dims_ref.write().await = (w, h);
                        let _ = video_tx.send(build_meta_frame(w, h));
                    }
                    if let Some(tx) = initial_dims_tx.take() {
                        let _ = tx.send((w, h));
                    }
                }

                if is_config {
                    *latest_cfg.write().await = Some(frame.clone());
                    *latest_key.write().await = None;
                } else if is_keyframe || annex_b_contains_idr(&frame[10..]) {
                    *latest_key.write().await = Some(frame.clone());
                }
                let _ = video_tx.send(frame);
            }
        });

        let ws_video_tx = self.video_tx.clone();
        let ws_dims = Arc::clone(&self.dimensions);
        let ws_latest_cfg = Arc::clone(&self.latest_config_frame);
        let ws_latest_key = Arc::clone(&self.latest_keyframe);
        let expected_token = ws_token.clone();
        let ws_accept_task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = ws_listener.accept().await else {
                    break;
                };
                let tx_sub = ws_video_tx.subscribe();
                let dims = Arc::clone(&ws_dims);
                let cfg_cache = Arc::clone(&ws_latest_cfg);
                let key_cache = Arc::clone(&ws_latest_key);
                let tok = expected_token.clone();
                tokio::spawn(async move {
                    let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                        let query = req.uri().query().unwrap_or("");
                        let authorized = query.split('&').any(|kv| {
                            kv.strip_prefix("token=")
                                .map(|provided| constant_time_eq(provided, tok.as_str()))
                                .unwrap_or(false)
                        });
                        if authorized {
                            Ok(resp)
                        } else {
                            let err_resp = tokio_tungstenite::tungstenite::http::Response::builder()
                                .status(403)
                                .body(Some("Forbidden".to_string()))
                                .unwrap();
                            Err(err_resp)
                        }
                    };
                    let Ok(mut ws) =
                        tokio_tungstenite::accept_hdr_async(tcp, callback).await
                    else {
                        return;
                    };

                    let (w, h) = *dims.read().await;
                    if ws
                        .send(Message::Binary(build_meta_frame(w, h).into()))
                        .await
                        .is_err()
                    {
                        return;
                    }

                    if let Some(cfg_frame) = cfg_cache.read().await.clone() {
                        if ws.send(Message::Binary(cfg_frame.into())).await.is_err() {
                            return;
                        }
                    }

                    // Immediately replay cached IDR keyframe so late-joining WebCodecs clients
                    // render right away instead of waiting up to 10s for the next MediaCodec IDR.
                    if let Some(key_frame) = key_cache.read().await.clone() {
                        if ws.send(Message::Binary(key_frame.into())).await.is_err() {
                            return;
                        }
                    }

                    let mut rx = tx_sub;
                    let (mut ws_sink, mut ws_stream) = ws.split();
                    let read_drain = tokio::spawn(async move {
                        while let Some(Ok(_)) = ws_stream.next().await {}
                    });

                    loop {
                        match rx.recv().await {
                            Ok(frame) => {
                                if ws_sink.send(Message::Binary(frame.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    read_drain.abort();
                });
            }
        });

        // Wait briefly for the first H.264 SPS frame so returned dimensions match the live stream
        let (width, height) = match tokio::time::timeout(Duration::from_millis(1500), initial_dims_rx)
            .await
        {
            Ok(Ok(dims)) => dims,
            _ => *self.dimensions.read().await,
        };

        if config.turn_screen_off {
            let _ = control_tx.try_send(vec![10, 0]);
        }

        let info = ScrcpySessionInfo {
            active: true,
            serial: serial.to_string(),
            ws_url: ws_url.clone(),
            ws_token: ws_token.clone(),
            video_width: width,
            video_height: height,
            scrcpy_version: version.clone(),
        };

        *self.session.lock().await = Some(ActiveSession {
            serial: serial.to_string(),
            local_port,
            ws_url,
            ws_token,
            scrcpy_version: version,
            server_child: Some(child),
            control_tx,
            tasks: vec![ctrl_write_task, ctrl_read_task, video_read_task, ws_accept_task],
            did_turn_screen_off: config.turn_screen_off,
        });

        Ok(info)
    }

    pub async fn stop(&self) {
        let maybe_session = self.session.lock().await.take();
        if let Some(mut s) = maybe_session {
            if s.did_turn_screen_off {
                let _ = s.control_tx.send(vec![10, 2]).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            for task in s.tasks {
                task.abort();
            }
            if let Some(mut child) = s.server_child.take() {
                let _ = child.kill().await;
            }
            let tcp_spec = format!("tcp:{}", s.local_port);
            let _ = self
                .adb_runner
                .execute(&["-s", &s.serial, "forward", "--remove", &tcp_spec], 2.0)
                .await;
        }
        *self.latest_config_frame.write().await = None;
        *self.latest_keyframe.write().await = None;
    }

    // MARK: - Control Message Encoders (scrcpy protocol v2-v5)

    pub fn encode_touch_event(
        action: u8,
        pointer_id: u64,
        x: i32,
        y: i32,
        screen_width: u16,
        screen_height: u16,
        pressure: f32,
    ) -> Vec<u8> {
        let mut msg = Vec::with_capacity(32);
        msg.push(2); // TYPE_INJECT_TOUCH_EVENT
        msg.push(action);
        msg.extend_from_slice(&pointer_id.to_be_bytes());
        msg.extend_from_slice(&x.to_be_bytes());
        msg.extend_from_slice(&y.to_be_bytes());
        msg.extend_from_slice(&screen_width.to_be_bytes());
        msg.extend_from_slice(&screen_height.to_be_bytes());
        let eff_pressure = if action == 1 || action == 3 {
            0.0
        } else {
            pressure.clamp(0.0, 1.0)
        };
        let p_u16 = (eff_pressure * 65535.0) as u16;
        msg.extend_from_slice(&p_u16.to_be_bytes());
        let action_button: u32 = if action == 2 { 0 } else { 1 };
        let buttons: u32 = if action == 1 || action == 3 { 0 } else { 1 };
        msg.extend_from_slice(&action_button.to_be_bytes());
        msg.extend_from_slice(&buttons.to_be_bytes());
        msg
    }

    pub fn encode_scroll_event(
        x: i32,
        y: i32,
        screen_width: u16,
        screen_height: u16,
        delta_h: f32,
        delta_v: f32,
    ) -> Vec<u8> {
        let mut msg = Vec::with_capacity(21);
        msg.push(3); // TYPE_INJECT_SCROLL_EVENT
        msg.extend_from_slice(&x.to_be_bytes());
        msg.extend_from_slice(&y.to_be_bytes());
        msg.extend_from_slice(&screen_width.to_be_bytes());
        msg.extend_from_slice(&screen_height.to_be_bytes());
        let h16 = (delta_h.clamp(-1.0, 1.0) * 32767.0) as i16;
        let v16 = (delta_v.clamp(-1.0, 1.0) * 32767.0) as i16;
        msg.extend_from_slice(&h16.to_be_bytes());
        msg.extend_from_slice(&v16.to_be_bytes());
        msg.extend_from_slice(&0u32.to_be_bytes());
        msg
    }

    pub fn encode_keycode(keycode: u32, action: u8, meta_state: u32) -> Vec<u8> {
        let mut msg = Vec::with_capacity(14);
        msg.push(0); // TYPE_INJECT_KEYCODE
        msg.push(action);
        msg.extend_from_slice(&keycode.to_be_bytes());
        msg.extend_from_slice(&0u32.to_be_bytes());
        msg.extend_from_slice(&meta_state.to_be_bytes());
        msg
    }

    pub async fn send_touch(
        &self,
        action: u8,
        pointer_id: u64,
        norm_x: f64,
        norm_y: f64,
    ) -> Result<(i32, i32), String> {
        let (w, h) = *self.dimensions.read().await;
        let x = (norm_x.clamp(0.0, 1.0) * (w as f64)).round() as i32;
        let y = (norm_y.clamp(0.0, 1.0) * (h as f64)).round() as i32;
        let clamped_x = x.clamp(0, w.saturating_sub(1) as i32);
        let clamped_y = y.clamp(0, h.saturating_sub(1) as i32);
        let msg = Self::encode_touch_event(
            action,
            pointer_id,
            clamped_x,
            clamped_y,
            w as u16,
            h as u16,
            1.0,
        );
        self.send_control_raw(msg).await?;
        Ok((clamped_x, clamped_y))
    }

    pub async fn send_scroll(
        &self,
        norm_x: f64,
        norm_y: f64,
        delta_h: f32,
        delta_v: f32,
    ) -> Result<(), String> {
        let (w, h) = *self.dimensions.read().await;
        let x = (norm_x.clamp(0.0, 1.0) * (w as f64)).round() as i32;
        let y = (norm_y.clamp(0.0, 1.0) * (h as f64)).round() as i32;
        let msg = Self::encode_scroll_event(x, y, w as u16, h as u16, delta_h, delta_v);
        self.send_control_raw(msg).await
    }

    pub async fn send_keycode(&self, keycode: u32) -> Result<(), String> {
        self.send_control_raw(Self::encode_keycode(keycode, 0, 0))
            .await?;
        self.send_control_raw(Self::encode_keycode(keycode, 1, 0))
            .await
    }

    pub async fn send_text(&self, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        if !text.is_ascii() {
            return self.set_clipboard(text, true).await;
        }
        let bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(5 + bytes.len());
        msg.push(1); // TYPE_INJECT_TEXT
        msg.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        msg.extend_from_slice(bytes);
        self.send_control_raw(msg).await
    }

    pub async fn set_clipboard(&self, text: &str, paste: bool) -> Result<(), String> {
        let mut seq_guard = self.clipboard_seq.lock().await;
        *seq_guard = seq_guard.wrapping_add(1);
        let seq = *seq_guard;
        drop(seq_guard);

        let bytes = text.as_bytes();
        let mut msg = Vec::with_capacity(14 + bytes.len());
        msg.push(9); // TYPE_SET_CLIPBOARD
        msg.extend_from_slice(&seq.to_be_bytes());
        msg.push(if paste { 1 } else { 0 });
        msg.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        msg.extend_from_slice(bytes);
        self.send_control_raw(msg).await
    }

    pub async fn set_screen_power_mode(&self, on: bool) -> Result<(), String> {
        // 0 = off; 2 = POWER_MODE_NORMAL (scrcpy 2.x) and non-zero boolean true (scrcpy 3.x-5.x)
        self.send_control_raw(vec![10, if on { 2 } else { 0 }])
            .await
    }

    async fn send_control_raw(&self, msg: Vec<u8>) -> Result<(), String> {
        let tx = self
            .session
            .lock()
            .await
            .as_ref()
            .map(|s| s.control_tx.clone())
            .ok_or_else(|| "Scrcpy session is not active".to_string())?;
        tx.send(msg)
            .await
            .map_err(|e| format!("Failed to send control message: {e}"))
    }

    async fn connect_video_socket(port: u16) -> Result<TcpStream, String> {
        let addr = format!("127.0.0.1:{port}");
        for _ in 0..40 {
            if let Ok(mut stream) = TcpStream::connect(&addr).await {
                let _ = stream.set_nodelay(true);
                let mut dummy = [0u8; 1];
                if let Ok(Ok(_)) =
                    tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut dummy))
                        .await
                {
                    return Ok(stream);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!(
            "Failed to connect to scrcpy video socket at port {port}"
        ))
    }

    async fn connect_control_socket(port: u16) -> Result<TcpStream, String> {
        let addr = format!("127.0.0.1:{port}");
        for _ in 0..25 {
            if let Ok(stream) = TcpStream::connect(&addr).await {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!(
            "Failed to connect to scrcpy control socket at port {port}"
        ))
    }
}

fn build_meta_frame(width: u32, height: u32) -> Vec<u8> {
    let mut meta = Vec::with_capacity(9);
    meta.push(WS_FRAME_META);
    meta.extend_from_slice(&width.to_be_bytes());
    meta.extend_from_slice(&height.to_be_bytes());
    meta
}

fn constant_time_eq(lhs: &str, rhs: &str) -> bool {
    let a = lhs.as_bytes();
    let b = rhs.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Minimal safe ZIP central-directory extractor supporting stored (0) and DEFLATE (8) entries.
fn extract_zip_entry(zip: &[u8], target_name: &str, max_uncompressed: usize) -> Option<Vec<u8>> {
    if zip.len() < 22 {
        return None;
    }
    let min_eocd = zip.len().saturating_sub(65_557);
    let mut eocd_pos = None;
    let mut i = zip.len() - 22;
    loop {
        if &zip[i..i + 4] == b"PK\x05\x06" {
            eocd_pos = Some(i);
            break;
        }
        if i <= min_eocd {
            break;
        }
        i -= 1;
    }
    let eocd = eocd_pos?;
    let total_entries = u16::from_le_bytes([zip[eocd + 10], zip[eocd + 11]]) as usize;
    let cd_size = u32::from_le_bytes([
        zip[eocd + 12],
        zip[eocd + 13],
        zip[eocd + 14],
        zip[eocd + 15],
    ]) as usize;
    let cd_offset = u32::from_le_bytes([
        zip[eocd + 16],
        zip[eocd + 17],
        zip[eocd + 18],
        zip[eocd + 19],
    ]) as usize;
    if cd_offset.checked_add(cd_size)? > zip.len() {
        return None;
    }

    let mut pos = cd_offset;
    for _ in 0..total_entries {
        if pos + 46 > cd_offset + cd_size || &zip[pos..pos + 4] != b"PK\x01\x02" {
            return None;
        }
        let method = u16::from_le_bytes([zip[pos + 10], zip[pos + 11]]);
        let comp_size = u32::from_le_bytes([
            zip[pos + 20],
            zip[pos + 21],
            zip[pos + 22],
            zip[pos + 23],
        ]) as usize;
        let uncomp_size = u32::from_le_bytes([
            zip[pos + 24],
            zip[pos + 25],
            zip[pos + 26],
            zip[pos + 27],
        ]) as usize;
        let name_len = u16::from_le_bytes([zip[pos + 28], zip[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([zip[pos + 30], zip[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([zip[pos + 32], zip[pos + 33]]) as usize;
        let local_hdr_offset = u32::from_le_bytes([
            zip[pos + 42],
            zip[pos + 43],
            zip[pos + 44],
            zip[pos + 45],
        ]) as usize;

        let name_start = pos + 46;
        let name_end = name_start.checked_add(name_len)?;
        if name_end > zip.len() {
            return None;
        }
        let name = std::str::from_utf8(&zip[name_start..name_end]).ok()?;
        pos = name_end.checked_add(extra_len)?.checked_add(comment_len)?;

        if name != target_name {
            continue;
        }
        if uncomp_size > max_uncompressed {
            return None;
        }
        if local_hdr_offset + 30 > zip.len()
            || &zip[local_hdr_offset..local_hdr_offset + 4] != b"PK\x03\x04"
        {
            return None;
        }
        let l_name_len =
            u16::from_le_bytes([zip[local_hdr_offset + 26], zip[local_hdr_offset + 27]]) as usize;
        let l_extra_len =
            u16::from_le_bytes([zip[local_hdr_offset + 28], zip[local_hdr_offset + 29]]) as usize;
        let data_start = local_hdr_offset
            .checked_add(30)?
            .checked_add(l_name_len)?
            .checked_add(l_extra_len)?;
        let data_end = data_start.checked_add(comp_size)?;
        if data_end > zip.len() {
            return None;
        }
        let raw = &zip[data_start..data_end];
        return match method {
            0 => Some(raw.to_vec()),
            8 => {
                let mut decoder = DeflateDecoder::new(raw);
                let mut out = Vec::with_capacity(uncomp_size.min(max_uncompressed));
                let mut limited = decoder.by_ref().take((max_uncompressed + 1) as u64);
                limited.read_to_end(&mut out).ok()?;
                if out.len() > max_uncompressed {
                    return None;
                }
                Some(out)
            }
            _ => None,
        };
    }
    None
}

// MARK: - H.264 Annex-B / SPS Dimension Parser

fn split_annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    if starts.is_empty() {
        return if data.is_empty() {
            Vec::new()
        } else {
            vec![data]
        };
    }
    let mut out = Vec::with_capacity(starts.len());
    for idx in 0..starts.len() {
        let payload_start = starts[idx].0 + starts[idx].1;
        let payload_end = if idx + 1 < starts.len() {
            starts[idx + 1].0
        } else {
            data.len()
        };
        if payload_end > payload_start {
            out.push(&data[payload_start..payload_end]);
        }
    }
    out
}

fn annex_b_contains_idr(data: &[u8]) -> bool {
    split_annex_b_nals(data)
        .iter()
        .any(|nal| !nal.is_empty() && (nal[0] & 0x1f) == 5)
}

/// Parses exact coded/cropped display `(width, height)` from an H.264 SPS NAL unit (ITU-T H.264 §7.3.2.1.1).
pub fn parse_h264_sps_dimensions(annex_b_or_raw: &[u8]) -> Option<(u32, u32)> {
    for nal in split_annex_b_nals(annex_b_or_raw) {
        if nal.len() < 4 || (nal[0] & 0x1f) != 7 {
            continue;
        }
        if let Some(dims) = parse_sps_nal_payload(&nal[1..]) {
            return Some(dims);
        }
    }
    None
}

fn strip_emulation_prevention(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut zeros = 0usize;
    for &b in raw {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0x00 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.data.len() {
            return None;
        }
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some(((self.data[byte_idx] >> shift) & 1) as u32)
    }

    fn read_bits(&mut self, count: usize) -> Option<u32> {
        if count > 32 {
            return None;
        }
        let mut val = 0u32;
        for _ in 0..count {
            val = (val << 1) | self.read_bit()?;
        }
        Some(val)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0usize;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        ((1u32).checked_shl(leading_zeros as u32)?)
            .checked_sub(1)?
            .checked_add(suffix)
    }

    fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let val = code.div_ceil(2) as i32;
        if code % 2 == 0 {
            Some(-val)
        } else {
            Some(val)
        }
    }

    fn skip_scaling_list(&mut self, size: usize) -> Option<()> {
        let mut last_scale = 8i32;
        let mut next_scale = 8i32;
        for _ in 0..size {
            if next_scale != 0 {
                let delta = self.read_se()?;
                next_scale = (last_scale + delta + 256) % 256;
            }
            last_scale = if next_scale == 0 {
                last_scale
            } else {
                next_scale
            };
        }
        Some(())
    }
}

fn parse_sps_nal_payload(payload_after_header: &[u8]) -> Option<(u32, u32)> {
    let rbsp = strip_emulation_prevention(payload_after_header);
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.read_bits(8)?;
    let _constraint_and_reserved = r.read_bits(8)?;
    let _level_idc = r.read_bits(8)?;
    let _sps_id = r.read_ue()?;

    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane_flag = 0u32;

    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane_flag = r.read_bit()?;
        }
        let _bit_depth_luma_minus8 = r.read_ue()?;
        let _bit_depth_chroma_minus8 = r.read_ue()?;
        let _qpprime_y_zero_transform_bypass = r.read_bit()?;
        let seq_scaling_matrix_present = r.read_bit()?;
        if seq_scaling_matrix_present == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.read_bit()? == 1 {
                    r.skip_scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let _log2_max_frame_num_minus4 = r.read_ue()?;
    let pic_order_cnt_type = r.read_ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_poc_lsb_minus4 = r.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero = r.read_bit()?;
        let _offset_for_non_ref_pic = r.read_se()?;
        let _offset_for_top_to_bottom_field = r.read_se()?;
        let num_ref_frames_in_poc_cycle = r.read_ue()?;
        if num_ref_frames_in_poc_cycle > 256 {
            return None;
        }
        for _ in 0..num_ref_frames_in_poc_cycle {
            let _ = r.read_se()?;
        }
    }

    let _max_num_ref_frames = r.read_ue()?;
    let _gaps_in_frame_num_allowed = r.read_bit()?;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field_flag = r.read_bit()?;
    }
    let _direct_8x8_inference_flag = r.read_bit()?;
    let frame_cropping_flag = r.read_bit()?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        (r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?)
    } else {
        (0, 0, 0, 0)
    };

    let chroma_array_type = if separate_colour_plane_flag == 0 {
        chroma_format_idc
    } else {
        0
    };
    let (crop_unit_x, crop_unit_y) = if chroma_array_type == 0 {
        (1u32, 2 - frame_mbs_only_flag)
    } else {
        let sub_width_c = if chroma_format_idc == 1 || chroma_format_idc == 2 {
            2u32
        } else {
            1u32
        };
        let sub_height_c = if chroma_format_idc == 1 { 2u32 } else { 1u32 };
        (sub_width_c, sub_height_c * (2 - frame_mbs_only_flag))
    };

    let raw_w = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let raw_h = (2 - frame_mbs_only_flag)
        .checked_mul(pic_height_in_map_units_minus1.checked_add(1)?)?
        .checked_mul(16)?;
    let crop_w = crop_left.checked_add(crop_right)?.checked_mul(crop_unit_x)?;
    let crop_h = crop_top.checked_add(crop_bottom)?.checked_mul(crop_unit_y)?;

    let width = raw_w.checked_sub(crop_w)?;
    let height = raw_h.checked_sub(crop_h)?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return None;
    }
    Some((width, height))
}
