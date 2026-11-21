use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

use crate::router::{MacroActionExecutor, PathDeviationError};
use crate::spatial_graph::{AttributedUIAction, UIActionKind};
use crate::transport::DaemonTcpClient;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroidPathDevice {
    pub serial: String,
    pub model: String,
    pub is_wireless: bool,
}

#[derive(Debug, Error)]
pub enum DroidPathAdbError {
    #[error("ADB executable not found. Install android-platform-tools or set ANDROID_HOME / DROIDPATH_ADB_PATH.")]
    BinaryNotFound,
    #[error("Invalid ADB device serial '{0}'.")]
    InvalidSerial(String),
    #[error("ADB failed (exit {exit_code}): {stderr}")]
    CommandFailed { exit_code: i32, stderr: String },
    #[error("ADB command timed out after {seconds:.1}s.")]
    TimedOut { seconds: f64 },
    #[error("IO error executing ADB: {0}")]
    Io(#[from] std::io::Error),
}

/// Cross-platform zero-dependency ADB subprocess runner (Windows, macOS, Linux)
/// with strict serial/package validation, safe POSIX quoting for Android shell, and timeout termination.
#[derive(Debug, Clone)]
pub struct DroidPathAdbRunner {
    cached_adb_path: Arc<Mutex<Option<PathBuf>>>,
}

impl Default for DroidPathAdbRunner {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DroidPathAdbRunner {
    pub fn new(adb_path: Option<PathBuf>) -> Self {
        Self {
            cached_adb_path: Arc::new(Mutex::new(adb_path)),
        }
    }

    pub fn validate_serial(serial: &str) -> bool {
        let trimmed = serial.trim();
        if trimmed.is_empty() || trimmed.len() > 128 || trimmed.starts_with('-') {
            return false;
        }
        trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'))
    }

    pub fn validate_package_name(package_name: &str) -> bool {
        let trimmed = package_name.trim();
        if trimmed.is_empty()
            || trimmed.len() > 255
            || trimmed.starts_with('-')
            || trimmed.starts_with('.')
            || trimmed.ends_with('.')
            || !trimmed.contains('.')
        {
            return false;
        }
        trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_'))
    }

    /// Safely single-quotes an arbitrary string for POSIX `/system/bin/sh` (Android `adb shell`),
    /// stripping NUL bytes so no metacharacter (`;`, `|`, `&`, `$()`, backticks, redirects) can execute.
    pub fn shell_quote(value: &str) -> String {
        let sanitized = value.replace('\0', "");
        format!("'{}'", sanitized.replace('\'', "'\\''"))
    }

    pub fn resolve_adb_path() -> Option<PathBuf> {
        let binary_name = if cfg!(windows) { "adb.exe" } else { "adb" };
        let mut candidates: Vec<PathBuf> = Vec::new();

        if let Ok(explicit) = std::env::var("DROIDPATH_ADB_PATH") {
            if !explicit.is_empty() {
                candidates.push(PathBuf::from(explicit));
            }
        }
        if let Ok(android_home) = std::env::var("ANDROID_HOME") {
            if !android_home.is_empty() {
                candidates.push(
                    PathBuf::from(android_home)
                        .join("platform-tools")
                        .join(binary_name),
                );
            }
        }
        if let Ok(sdk_root) = std::env::var("ANDROID_SDK_ROOT") {
            if !sdk_root.is_empty() {
                candidates.push(
                    PathBuf::from(sdk_root)
                        .join("platform-tools")
                        .join(binary_name),
                );
            }
        }

        if cfg!(windows) {
            if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
                candidates.push(
                    PathBuf::from(local_app_data)
                        .join("Android")
                        .join("Sdk")
                        .join("platform-tools")
                        .join("adb.exe"),
                );
            }
            if let Some(home) = dirs::home_dir() {
                candidates.push(
                    home.join("AppData")
                        .join("Local")
                        .join("Android")
                        .join("Sdk")
                        .join("platform-tools")
                        .join("adb.exe"),
                );
                candidates.push(home.join("scoop").join("shims").join("adb.exe"));
            }
            candidates.push(PathBuf::from(r"C:\platform-tools\adb.exe"));
        } else if cfg!(target_os = "macos") {
            if let Some(home) = dirs::home_dir() {
                candidates.push(
                    home.join("Library")
                        .join("Android")
                        .join("sdk")
                        .join("platform-tools")
                        .join("adb"),
                );
            }
            candidates.push(PathBuf::from("/opt/homebrew/bin/adb"));
            candidates.push(PathBuf::from("/usr/local/bin/adb"));
            candidates.push(PathBuf::from(
                "/Applications/AndOnMac.app/Contents/Resources/bin/adb",
            ));
        } else {
            if let Some(home) = dirs::home_dir() {
                candidates.push(
                    home.join("Android")
                        .join("Sdk")
                        .join("platform-tools")
                        .join("adb"),
                );
            }
            candidates.push(PathBuf::from("/usr/bin/adb"));
            candidates.push(PathBuf::from("/usr/local/bin/adb"));
        }

        if let Some(path_os) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_os) {
                candidates.push(dir.join(binary_name));
            }
        }

        candidates.into_iter().find(|p| Self::is_executable(p))
    }

    fn is_executable(path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            (meta.permissions().mode() & 0o111) != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    pub async fn adb_binary_path(&self) -> Result<PathBuf, DroidPathAdbError> {
        let mut guard = self.cached_adb_path.lock().await;
        if let Some(ref p) = *guard {
            if Self::is_executable(p) {
                return Ok(p.clone());
            }
        }
        let resolved = Self::resolve_adb_path().ok_or(DroidPathAdbError::BinaryNotFound)?;
        *guard = Some(resolved.clone());
        Ok(resolved)
    }

    pub async fn execute_bytes(
        &self,
        arguments: &[&str],
        timeout_secs: f64,
    ) -> Result<Vec<u8>, DroidPathAdbError> {
        let adb_path = self.adb_binary_path().await?;
        let mut cmd = Command::new(&adb_path);
        cmd.args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd.spawn()?;
        let timeout = Duration::from_secs_f64(timeout_secs.max(0.2));
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(DroidPathAdbError::TimedOut {
                    seconds: timeout_secs,
                });
            }
        };

        if !output.status.success() {
            let exit_code = output.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DroidPathAdbError::CommandFailed { exit_code, stderr });
        }

        Ok(output.stdout)
    }

    pub async fn execute(
        &self,
        arguments: &[&str],
        timeout_secs: f64,
    ) -> Result<String, DroidPathAdbError> {
        let bytes = self.execute_bytes(arguments, timeout_secs).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn list_devices(&self) -> Result<Vec<DroidPathDevice>, DroidPathAdbError> {
        let output = self.execute(&["devices", "-l"], 4.0).await?;
        Ok(Self::parse_devices_output(&output))
    }

    pub fn parse_devices_output(output: &str) -> Vec<DroidPathDevice> {
        let mut devices = Vec::new();
        for raw_line in output.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 || parts[1] != "device" {
                continue;
            }
            let serial = parts[0];
            if !Self::validate_serial(serial) {
                continue;
            }
            let model = parts
                .iter()
                .find_map(|p| p.strip_prefix("model:"))
                .map(|m| m.replace('_', " "))
                .unwrap_or_else(|| serial.to_string());
            let is_wireless = serial.contains(':') || serial.contains("_adb-tls-connect");
            devices.push(DroidPathDevice {
                serial: serial.to_string(),
                model,
                is_wireless,
            });
        }
        devices
    }

    pub async fn resolve_target_serial(
        &self,
        requested: Option<&str>,
    ) -> Result<String, DroidPathAdbError> {
        if let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) {
            if !Self::validate_serial(req) {
                return Err(DroidPathAdbError::InvalidSerial(req.to_string()));
            }
            return Ok(req.to_string());
        }
        let devices = self.list_devices().await?;
        devices
            .into_iter()
            .next()
            .map(|d| d.serial)
            .ok_or_else(|| DroidPathAdbError::CommandFailed {
                exit_code: 1,
                stderr: "No authorized Android devices found via adb devices.".to_string(),
            })
    }

    pub fn build_safe_type_text_command(raw_text: &str) -> (bool, String) {
        let sanitized = raw_text.replace('\0', "");
        let is_simple_ascii = sanitized
            .chars()
            .all(|c| c.is_ascii() && (c as u32) >= 0x20 && (c as u32) != 0x7F);
        if is_simple_ascii {
            let space_encoded = sanitized.replace(' ', "%s");
            (false, Self::shell_quote(&space_encoded))
        } else {
            (true, Self::shell_quote(&sanitized))
        }
    }

    pub fn resolve_key_code(raw: &str) -> Option<String> {
        let trimmed_upper = raw.trim().to_uppercase();
        let trimmed = trimmed_upper
            .strip_prefix("KEYCODE_")
            .unwrap_or(&trimmed_upper);
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Some(trimmed.to_string());
        }
        let mapped = match trimmed {
            "BACK" => "4",
            "HOME" => "3",
            "RECENTS" | "APP_SWITCH" => "187",
            "ENTER" => "66",
            "DELETE" => "67",
            "VOLUME_UP" => "24",
            "VOLUME_DOWN" => "25",
            "POWER" => "26",
            "WAKEUP" => "224",
            "NOTIFICATION" => "83",
            "PASTE" => "279",
            _ => return None,
        };
        Some(mapped.to_string())
    }
}

pub struct DroidPathActionExecutor {
    adb_runner: DroidPathAdbRunner,
    daemon_client: Option<Arc<DaemonTcpClient>>,
    daemon_active_serial: Option<Arc<RwLock<Option<String>>>>,
}

impl DroidPathActionExecutor {
    pub const DEFAULT_DAEMON_SWIPE_DURATION_MS: i32 = 145;
    pub const DEFAULT_ZERO_FLING_HOLD_MS: i32 = 25;

    pub fn new(adb_runner: DroidPathAdbRunner) -> Self {
        Self {
            adb_runner,
            daemon_client: None,
            daemon_active_serial: None,
        }
    }

    pub fn with_daemon(
        adb_runner: DroidPathAdbRunner,
        daemon_client: Arc<DaemonTcpClient>,
        daemon_active_serial: Option<Arc<RwLock<Option<String>>>>,
    ) -> Self {
        Self {
            adb_runner,
            daemon_client: Some(daemon_client),
            daemon_active_serial,
        }
    }

    async fn daemon_client_for_serial(&self, serial: &str) -> Option<&Arc<DaemonTcpClient>> {
        let client = self.daemon_client.as_ref()?;
        if let Some(ref active_serial) = self.daemon_active_serial {
            let guard = active_serial.read().await;
            if let Some(ref bound) = *guard {
                if !bound.is_empty() && bound != serial {
                    return None;
                }
            }
        }
        if client.supports_native_input().await {
            Some(client)
        } else {
            None
        }
    }

    pub async fn dispatch_tap(
        &self,
        serial: &str,
        x: i32,
        y: i32,
    ) -> Result<(), DroidPathAdbError> {
        if let Some(client) = self.daemon_client_for_serial(serial).await {
            if client.inject_tap(x, y).await.is_ok() {
                return Ok(());
            }
        }
        let xs = x.to_string();
        let ys = y.to_string();
        self.adb_runner
            .execute(&["-s", serial, "shell", "input", "tap", &xs, &ys], 5.0)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_swipe(
        &self,
        serial: &str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: Option<i32>,
        hold_ms: i32,
    ) -> Result<(), DroidPathAdbError> {
        if let Some(client) = self.daemon_client_for_serial(serial).await {
            let daemon_dur = duration_ms
                .unwrap_or(Self::DEFAULT_DAEMON_SWIPE_DURATION_MS)
                .clamp(20, 5000);
            if client
                .inject_swipe(x1, y1, x2, y2, daemon_dur, hold_ms.clamp(0, 1000))
                .await
                .is_ok()
            {
                return Ok(());
            }
        }
        let dur = duration_ms.unwrap_or(280).clamp(120, 5000).to_string();
        let (x1s, y1s, x2s, y2s) = (
            x1.to_string(),
            y1.to_string(),
            x2.to_string(),
            y2.to_string(),
        );
        self.adb_runner
            .execute(
                &[
                    "-s", serial, "shell", "input", "swipe", &x1s, &y1s, &x2s, &y2s, &dur,
                ],
                6.0,
            )
            .await?;
        Ok(())
    }

    pub async fn dispatch_keyevent_code(
        &self,
        serial: &str,
        key_code: &str,
    ) -> Result<(), DroidPathAdbError> {
        if let Ok(kc_num) = key_code.parse::<i32>() {
            if let Some(client) = self.daemon_client_for_serial(serial).await {
                if client.inject_keyevent(kc_num).await.is_ok() {
                    return Ok(());
                }
            }
        }
        self.adb_runner
            .execute(&["-s", serial, "shell", "input", "keyevent", key_code], 4.0)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl MacroActionExecutor for DroidPathActionExecutor {
    async fn supports_zero_fling_swipes(&self, serial: &str) -> bool {
        self.daemon_client_for_serial(serial).await.is_some()
    }

    async fn execute(
        &self,
        action: &AttributedUIAction,
        serial: &str,
    ) -> Result<(), PathDeviationError> {
        let sig = action.signature();
        let map_err = |e: DroidPathAdbError| PathDeviationError::ActionDispatchFailed {
            step_index: -1,
            action_signature: sig.clone(),
            underlying_message: e.to_string(),
        };

        match action.kind {
            UIActionKind::Tap => {
                let (Some(x), Some(y)) = (action.x, action.y) else {
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index: -1,
                        action_signature: sig,
                        underlying_message: "Missing tap coordinates (x, y).".to_string(),
                    });
                };
                self.dispatch_tap(serial, x, y).await.map_err(map_err)?;
            }

            UIActionKind::Swipe => {
                let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                    (action.x, action.y, action.end_x, action.end_y)
                else {
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index: -1,
                        action_signature: sig,
                        underlying_message: "Missing swipe coordinates.".to_string(),
                    });
                };
                let is_banner_dismiss = action
                    .target_description
                    .starts_with("Dismiss occluding notification");
                let hold_ms = if is_banner_dismiss {
                    0
                } else {
                    Self::DEFAULT_ZERO_FLING_HOLD_MS
                };
                let is_recorded_scroll = action.target_description.starts_with("explore_scroll_")
                    || action.target_description.starts_with("Smart scroll ");
                let daemon_ready = self.daemon_client_for_serial(serial).await.is_some();
                let effective_dur = if daemon_ready && is_recorded_scroll {
                    Some(
                        action
                            .duration_ms
                            .unwrap_or(Self::DEFAULT_DAEMON_SWIPE_DURATION_MS)
                            .clamp(50, 155),
                    )
                } else {
                    action.duration_ms
                };
                self.dispatch_swipe(serial, x1, y1, x2, y2, effective_dur, hold_ms)
                    .await
                    .map_err(map_err)?;
            }

            UIActionKind::Keyevent => {
                let Some(key_code) =
                    DroidPathAdbRunner::resolve_key_code(&action.target_description)
                else {
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index: -1,
                        action_signature: sig,
                        underlying_message: format!(
                            "Unsupported or unsafe keyevent '{}'.",
                            action.target_description
                        ),
                    });
                };
                self.dispatch_keyevent_code(serial, &key_code)
                    .await
                    .map_err(map_err)?;
            }

            UIActionKind::TypeText => {
                let text = action
                    .text_input
                    .as_deref()
                    .unwrap_or(action.target_description.as_str());
                if text.is_empty() {
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index: -1,
                        action_signature: sig,
                        underlying_message: "Missing textInput payload.".to_string(),
                    });
                }
                let (use_clipboard, quoted) =
                    DroidPathAdbRunner::build_safe_type_text_command(text);
                if !use_clipboard {
                    self.adb_runner
                        .execute(&["-s", serial, "shell", "input", "text", &quoted], 5.0)
                        .await
                        .map_err(map_err)?;
                } else {
                    self.adb_runner
                        .execute(
                            &["-s", serial, "shell", "cmd", "clipboard", "set", "text", &quoted],
                            4.0,
                        )
                        .await
                        .map_err(map_err)?;
                    self.dispatch_keyevent_code(serial, "279")
                        .await
                        .map_err(map_err)?;
                }
                if action.press_enter == Some(true) {
                    self.dispatch_keyevent_code(serial, "66")
                        .await
                        .map_err(map_err)?;
                }
            }

            UIActionKind::LaunchApp => {
                let pkg = action.target_description.trim();
                if !DroidPathAdbRunner::validate_package_name(pkg) {
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index: -1,
                        action_signature: sig,
                        underlying_message: format!("Invalid package name '{pkg}'."),
                    });
                }
                self.adb_runner
                    .execute(
                        &[
                            "-s",
                            serial,
                            "shell",
                            "monkey",
                            "-p",
                            pkg,
                            "-c",
                            "android.intent.category.LAUNCHER",
                            "1",
                        ],
                        6.0,
                    )
                    .await
                    .map_err(map_err)?;
            }
        }
        Ok(())
    }
}
