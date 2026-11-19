use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::Utc;
use droidpath_core::{
    AttributedUIAction, DroidPathAdbRunner, DroidPathDevice, DroidPathGraphStats,
    DroidPathHttpServer, DroidPathManager, DroidPathMcpServerCore,
    DroidPathPersistenceCoordinator, HazardousConfirmationRequest, MacroExecutionEvent,
    ScrcpyBridge, ScrcpySessionInfo, ScrcpyStartConfig, ShadowCompactNode, ShadowNavigationOutcome,
    SpatialGraphSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct PendingHitlNavigation {
    serial: String,
    target: Option<String>,
    target_hash: Option<u64>,
    max_steps: usize,
    step_timeout_ms: u64,
    request: HazardousConfirmationRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopGraphStatsDto {
    pub vertex_count: usize,
    pub edge_count: usize,
    pub current_state_hash_hex: Option<String>,
    pub current_package: String,
    pub connection_state: droidpath_core::DaemonConnectionState,
    pub has_fresh_shadow_dom: bool,
    pub storage_path: String,
}

impl From<DroidPathGraphStats> for DesktopGraphStatsDto {
    fn from(s: DroidPathGraphStats) -> Self {
        Self {
            vertex_count: s.vertex_count,
            edge_count: s.edge_count,
            current_state_hash_hex: s.current_state_hash.map(|h| format!("0x{h:016x}")),
            current_package: s.current_package,
            connection_state: s.connection_state,
            has_fresh_shadow_dom: s.has_fresh_shadow_dom,
            storage_path: s.storage_path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopVertexDto {
    pub layout_hash_hex: String,
    pub active_package: String,
    pub sample_interactive_labels: Vec<String>,
    pub visit_count: i32,
    pub discovery_parent_hash_hex: Option<String>,
    pub discovery_depth: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopEdgeDto {
    pub id: String,
    pub from_hash_hex: String,
    pub to_hash_hex: String,
    pub action_signature: String,
    pub traversal_count: i32,
    pub is_hazardous: bool,
    pub hazard_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopGraphSnapshotDto {
    pub root_state_hash_hex: Option<String>,
    pub current_state_hash_hex: Option<String>,
    pub vertices: Vec<DesktopVertexDto>,
    pub edges: Vec<DesktopEdgeDto>,
}

impl From<SpatialGraphSnapshot> for DesktopGraphSnapshotDto {
    fn from(snap: SpatialGraphSnapshot) -> Self {
        Self {
            root_state_hash_hex: snap.root_state_hash.map(|h| format!("0x{h:016x}")),
            current_state_hash_hex: snap.current_state_hash.map(|h| format!("0x{h:016x}")),
            vertices: snap
                .vertices
                .into_iter()
                .map(|v| DesktopVertexDto {
                    layout_hash_hex: format!("0x{:016x}", v.layout_hash),
                    active_package: v.active_package,
                    sample_interactive_labels: v.sample_interactive_labels,
                    visit_count: v.visit_count,
                    discovery_parent_hash_hex: v
                        .discovery_parent_hash
                        .map(|h| format!("0x{h:016x}")),
                    discovery_depth: v.discovery_depth,
                })
                .collect(),
            edges: snap
                .edges
                .into_iter()
                .map(|e| DesktopEdgeDto {
                    id: e.id.to_string().to_uppercase(),
                    from_hash_hex: format!("0x{:016x}", e.from_hash),
                    to_hash_hex: format!("0x{:016x}", e.to_hash),
                    action_signature: e.action_signature,
                    traversal_count: e.traversal_count,
                    is_hazardous: e.is_hazardous,
                    hazard_reason: e.hazard_reason,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectDeviceResult {
    pub serial: String,
    pub daemon_ready: bool,
    pub daemon_warning: Option<String>,
    pub mirror: Option<ScrcpySessionInfo>,
    pub mirror_mode: String,
    pub mirror_warning: Option<String>,
    pub video_width: u32,
    pub video_height: u32,
    pub display_width: u32,
    pub display_height: u32,
}

/// Aligns physical Android display dimensions (`wm size`) to match the current video stream
/// orientation (portrait vs landscape) so passive-learning Shadow DOM lookup and HUD overlays
/// map accurately even when `scrcpy-server` downscales video via `max_size` or the phone rotates.
pub fn orient_display_dimensions(display: (u32, u32), video: (u32, u32)) -> (u32, u32) {
    let (dw, dh) = display;
    let (vw, vh) = video;
    if dw == 0 || dh == 0 {
        return video;
    }
    if vw == 0 || vh == 0 {
        return display;
    }
    let display_landscape = dw > dh;
    let video_landscape = vw > vh;
    if display_landscape != video_landscape {
        (dh, dw)
    } else {
        (dw, dh)
    }
}

/// Maps normalized `[0, 1]` pointer coordinates onto oriented physical Android display pixels.
pub fn normalized_to_physical_display(
    norm_x: f64,
    norm_y: f64,
    display: (u32, u32),
    video: (u32, u32),
) -> (i32, i32) {
    let (w, h) = orient_display_dimensions(display, video);
    let x = (norm_x.clamp(0.0, 1.0) * (w as f64)).round() as i32;
    let y = (norm_y.clamp(0.0, 1.0) * (h as f64)).round() as i32;
    (
        x.clamp(0, w.saturating_sub(1) as i32),
        y.clamp(0, h.saturating_sub(1) as i32),
    )
}

/// Extracts `(width, height)` from a PNG IHDR chunk header so snapshot-fallback mode
/// tracks portrait <-> landscape rotation accurately even when `scrcpy` is inactive.
pub fn parse_png_dimensions(png_bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png_bytes.len() < 24 || &png_bytes[0..8] != PNG_SIG || &png_bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([png_bytes[16], png_bytes[17], png_bytes[18], png_bytes[19]]);
    let h = u32::from_be_bytes([png_bytes[20], png_bytes[21], png_bytes[22], png_bytes[23]]);
    if w > 0 && h > 0 && w <= 16_384 && h <= 16_384 {
        Some((w, h))
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PassiveLearningEvent {
    pub kind: String,
    pub label: String,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub selector_text: Option<String>,
    pub selector_resource_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub sse_url: Option<String>,
}

pub struct AppState {
    pub adb: DroidPathAdbRunner,
    pub manager: Arc<DroidPathManager>,
    pub scrcpy: Arc<ScrcpyBridge>,
    pub mcp_http: Arc<DroidPathHttpServer>,
    pub mcp_http_port: Mutex<Option<u16>>,
    pub active_serial: Mutex<Option<String>>,
    pub display_dimensions: Mutex<(u32, u32)>,
    pub(crate) pending_hitl: Mutex<Option<PendingHitlNavigation>>,
}

#[tauri::command]
async fn list_devices(state: State<'_, Arc<AppState>>) -> Result<Vec<DroidPathDevice>, String> {
    state.adb.list_devices().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn connect_device(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    serial: Option<String>,
    deploy_daemon: Option<bool>,
    start_mirror: Option<bool>,
) -> Result<ConnectDeviceResult, String> {
    let target_serial = state
        .adb
        .resolve_target_serial(serial.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    *state.active_serial.lock().await = Some(target_serial.clone());

    let mut daemon_ready = false;
    let mut daemon_warning = None;
    if deploy_daemon.unwrap_or(true) {
        let port = state.manager.bridge.client.configuration().port;
        match state
            .manager
            .deploy_and_launch_daemon(&target_serial, port)
            .await
        {
            Ok(_) => daemon_ready = true,
            Err(err) => daemon_warning = Some(err),
        }
    }

    state
        .manager
        .ensure_started(Some(&target_serial), true)
        .await;

    // Always query physical Android display dimensions (`wm size`) so passive learning,
    // HUD reticles, and ADB fallback operate in real Android Shadow DOM pixel space
    // even when scrcpy downscales video via `max_size`.
    let wm_size = query_device_wm_size(&state.adb, &target_serial).await.ok();

    let mut mirror = None;
    let mut mirror_mode = "snapshot".to_string();
    let mut mirror_warning = None;
    let (mut video_width, mut video_height) = wm_size.unwrap_or((1080u32, 2400u32));

    if start_mirror.unwrap_or(true) {
        match state
            .scrcpy
            .start(&target_serial, ScrcpyStartConfig::default())
            .await
        {
            Ok(info) => {
                video_width = info.video_width;
                video_height = info.video_height;
                mirror_mode = "webcodecs".to_string();
                mirror = Some(info);
            }
            Err(err) => {
                mirror_warning = Some(err);
            }
        }
    }

    let raw_display = wm_size.unwrap_or((video_width, video_height));
    *state.display_dimensions.lock().await = raw_display;
    let (display_width, display_height) =
        orient_display_dimensions(raw_display, (video_width, video_height));

    let stats = DesktopGraphStatsDto::from(state.manager.graph_stats().await);
    let _ = app.emit("graph-stats", &stats);

    Ok(ConnectDeviceResult {
        serial: target_serial,
        daemon_ready,
        daemon_warning,
        mirror,
        mirror_mode,
        mirror_warning,
        video_width,
        video_height,
        display_width,
        display_height,
    })
}

#[tauri::command]
async fn disconnect_device(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.scrcpy.stop().await;
    state.manager.stop().await;
    *state.active_serial.lock().await = None;
    *state.pending_hitl.lock().await = None;
    Ok(())
}

#[tauri::command]
async fn get_graph_stats(
    state: State<'_, Arc<AppState>>,
) -> Result<DesktopGraphStatsDto, String> {
    Ok(DesktopGraphStatsDto::from(state.manager.graph_stats().await))
}

#[tauri::command]
async fn get_graph_snapshot(
    state: State<'_, Arc<AppState>>,
) -> Result<DesktopGraphSnapshotDto, String> {
    state.manager.load_persisted_graph_if_needed().await;
    Ok(DesktopGraphSnapshotDto::from(
        state.manager.bridge.graph_engine.snapshot().await,
    ))
}

#[tauri::command]
async fn get_ui_tree(state: State<'_, Arc<AppState>>) -> Result<Vec<ShadowCompactNode>, String> {
    Ok(state.manager.bridge.get_compact_tree().await)
}

#[tauri::command]
async fn navigate_to_node(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    target: Option<String>,
    target_hash: Option<String>,
    confirmed: Option<bool>,
    max_steps: Option<usize>,
    step_timeout_ms: Option<u64>,
) -> Result<Value, String> {
    let active_serial = state.active_serial.lock().await.clone();
    let serial = state
        .adb
        .resolve_target_serial(active_serial.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    let parsed_hash = target_hash
        .as_deref()
        .map(Value::from)
        .as_ref()
        .and_then(|v| DroidPathMcpServerCore::parse_hash(Some(v)));
    let max_s = max_steps.unwrap_or(12).clamp(1, 30);
    let timeout_ms = step_timeout_ms.unwrap_or(2000).clamp(250, 10_000);
    let is_confirmed = confirmed.unwrap_or(false);

    if is_confirmed {
        *state.pending_hitl.lock().await = None;
    }

    let outcome = state
        .manager
        .navigate_to_node(
            &serial,
            target.clone(),
            parsed_hash,
            max_s,
            timeout_ms,
            is_confirmed,
            60.0,
            None,
        )
        .await;

    let stats = DesktopGraphStatsDto::from(state.manager.graph_stats().await);
    let _ = app.emit("graph-stats", &stats);

    match outcome {
        Ok(ShadowNavigationOutcome::RequiresConfirmation(ref req)) => {
            *state.pending_hitl.lock().await = Some(PendingHitlNavigation {
                serial,
                target,
                target_hash: parsed_hash.or(Some(req.target_hash)),
                max_steps: max_s,
                step_timeout_ms: timeout_ms,
                request: req.clone(),
            });
            let formatted = DroidPathMcpServerCore::format_navigation_outcome(
                &ShadowNavigationOutcome::RequiresConfirmation(req.clone()),
            );
            let val: Value =
                serde_json::from_str(&formatted.text).unwrap_or_else(|_| json!({ "raw": formatted.text }));
            Ok(val)
        }
        Ok(other) => {
            *state.pending_hitl.lock().await = None;
            let formatted = DroidPathMcpServerCore::format_navigation_outcome(&other);
            let val: Value =
                serde_json::from_str(&formatted.text).unwrap_or_else(|_| json!({ "raw": formatted.text }));
            Ok(val)
        }
        Err(deviation) => {
            let formatted = DroidPathMcpServerCore::format_path_deviation_result(&deviation);
            let val: Value =
                serde_json::from_str(&formatted.text).unwrap_or_else(|_| json!({ "raw": formatted.text }));
            Ok(val)
        }
    }
}

#[tauri::command]
async fn resolve_hitl_confirmation(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    approved: bool,
) -> Result<Value, String> {
    let pending = state.pending_hitl.lock().await.take();
    let Some(nav) = pending else {
        return Ok(json!({ "status": "no_pending_confirmation" }));
    };

    if !approved {
        let _ = app.emit(
            "hud-event",
            &MacroExecutionEvent::Failed {
                message: format!(
                    "Cancelled hazardous action '{}'",
                    nav.request.target_description
                ),
            },
        );
        return Ok(json!({
            "status": "cancelled",
            "action": nav.request.action_signature
        }));
    }

    let outcome = state
        .manager
        .navigate_to_node(
            &nav.serial,
            nav.target,
            nav.target_hash,
            nav.max_steps,
            nav.step_timeout_ms,
            true,
            60.0,
            None,
        )
        .await;

    let stats = DesktopGraphStatsDto::from(state.manager.graph_stats().await);
    let _ = app.emit("graph-stats", &stats);

    match outcome {
        Ok(other) => {
            let formatted = DroidPathMcpServerCore::format_navigation_outcome(&other);
            Ok(serde_json::from_str(&formatted.text)
                .unwrap_or_else(|_| json!({ "raw": formatted.text })))
        }
        Err(deviation) => {
            let formatted = DroidPathMcpServerCore::format_path_deviation_result(&deviation);
            Ok(serde_json::from_str(&formatted.text)
                .unwrap_or_else(|_| json!({ "raw": formatted.text })))
        }
    }
}

#[tauri::command]
async fn inject_touch(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    action: u8,
    norm_x: f64,
    norm_y: f64,
    record_passive_tap: Option<bool>,
) -> Result<(), String> {
    let display_dims = *state.display_dimensions.lock().await;
    let video_dims = state
        .scrcpy
        .session_info()
        .await
        .map(|s| (s.video_width, s.video_height))
        .unwrap_or(display_dims);
    let (dev_x, dev_y) = normalized_to_physical_display(norm_x, norm_y, display_dims, video_dims);

    if record_passive_tap.unwrap_or(false) {
        let attributed = if let Some(idx) = state.manager.bridge.dom_state.current_index().await {
            idx.attributed_tap_action(dev_x, dev_y, Utc::now())
        } else {
            AttributedUIAction::tap(format!("({dev_x},{dev_y})"), Some(dev_x), Some(dev_y))
        };
        let event = PassiveLearningEvent {
            kind: "tap".to_string(),
            label: attributed.target_description.clone(),
            x: attributed.x,
            y: attributed.y,
            selector_text: attributed.selector_text.clone(),
            selector_resource_id: attributed.selector_resource_id.clone(),
        };
        state.manager.record_action(attributed).await;
        let _ = app.emit("passive-learning", &event);
    }

    if state.scrcpy.send_touch(action, 0, norm_x, norm_y).await.is_err() && action == 1 {
        // Fallback: dispatch tap via ADB when scrcpy mirror isn't connected
        if let Some(serial) = state.active_serial.lock().await.clone() {
            let xs = dev_x.to_string();
            let ys = dev_y.to_string();
            let _ = state
                .adb
                .execute(&["-s", &serial, "shell", "input", "tap", &xs, &ys], 4.0)
                .await;
        }
    }
    Ok(())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn inject_swipe(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    start_norm_x: f64,
    start_norm_y: f64,
    end_norm_x: f64,
    end_norm_y: f64,
    duration_ms: Option<i32>,
    dispatch_via_adb: Option<bool>,
) -> Result<(), String> {
    let display_dims = *state.display_dimensions.lock().await;
    let video_dims = state
        .scrcpy
        .session_info()
        .await
        .map(|s| (s.video_width, s.video_height))
        .unwrap_or(display_dims);
    let (x1, y1) =
        normalized_to_physical_display(start_norm_x, start_norm_y, display_dims, video_dims);
    let (x2, y2) = normalized_to_physical_display(end_norm_x, end_norm_y, display_dims, video_dims);
    let dur = duration_ms.unwrap_or(280).clamp(80, 3000);

    let label = format!("swipe ({x1},{y1})->({x2},{y2})");
    state
        .manager
        .record_action(AttributedUIAction::swipe(
            label.clone(),
            x1,
            y1,
            x2,
            y2,
            Some(dur),
        ))
        .await;
    let _ = app.emit(
        "passive-learning",
        &PassiveLearningEvent {
            kind: "swipe".to_string(),
            label,
            x: Some(x2),
            y: Some(y2),
            selector_text: None,
            selector_resource_id: None,
        },
    );

    if dispatch_via_adb.unwrap_or(false) {
        if let Some(serial) = state.active_serial.lock().await.clone() {
            let (x1s, y1s, x2s, y2s, ds) = (
                x1.to_string(),
                y1.to_string(),
                x2.to_string(),
                y2.to_string(),
                dur.to_string(),
            );
            let _ = state
                .adb
                .execute(
                    &[
                        "-s", &serial, "shell", "input", "swipe", &x1s, &y1s, &x2s, &y2s, &ds,
                    ],
                    5.0,
                )
                .await;
        }
    }
    Ok(())
}

#[tauri::command]
async fn inject_scroll(
    state: State<'_, Arc<AppState>>,
    norm_x: f64,
    norm_y: f64,
    delta_h: f32,
    delta_v: f32,
) -> Result<(), String> {
    let _ = state
        .scrcpy
        .send_scroll(norm_x, norm_y, delta_h, delta_v)
        .await;
    Ok(())
}

#[tauri::command]
async fn inject_keyevent(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    keycode: String,
) -> Result<(), String> {
    let Some(resolved) = DroidPathAdbRunner::resolve_key_code(&keycode) else {
        return Err(format!("Unsupported keycode '{keycode}'"));
    };
    state
        .manager
        .record_action(AttributedUIAction::keyevent(keycode.to_uppercase()))
        .await;
    let _ = app.emit(
        "passive-learning",
        &PassiveLearningEvent {
            kind: "keyevent".to_string(),
            label: keycode.to_uppercase(),
            x: None,
            y: None,
            selector_text: None,
            selector_resource_id: None,
        },
    );

    if let Ok(kc_num) = resolved.parse::<u32>() {
        if state.scrcpy.send_keycode(kc_num).await.is_ok() {
            return Ok(());
        }
    }
    if let Some(serial) = state.active_serial.lock().await.clone() {
        state
            .adb
            .execute(&["-s", &serial, "shell", "input", "keyevent", &resolved], 4.0)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
async fn inject_text(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    text: String,
    press_enter: Option<bool>,
) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let enter = press_enter.unwrap_or(false);
    state
        .manager
        .record_action(AttributedUIAction::type_text(text.clone(), enter))
        .await;
    let _ = app.emit(
        "passive-learning",
        &PassiveLearningEvent {
            kind: "typeText".to_string(),
            label: text.chars().take(24).collect(),
            x: None,
            y: None,
            selector_text: None,
            selector_resource_id: None,
        },
    );

    if state.scrcpy.send_text(&text).await.is_ok() {
        if enter {
            let _ = state.scrcpy.send_keycode(66).await;
        }
        return Ok(());
    }

    if let Some(serial) = state.active_serial.lock().await.clone() {
        let (use_clip, quoted) = DroidPathAdbRunner::build_safe_type_text_command(&text);
        if !use_clip {
            state
                .adb
                .execute(&["-s", &serial, "shell", "input", "text", &quoted], 5.0)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            state
                .adb
                .execute(
                    &["-s", &serial, "shell", "cmd", "clipboard", "set", "text", &quoted],
                    4.0,
                )
                .await
                .map_err(|e| e.to_string())?;
            state
                .adb
                .execute(&["-s", &serial, "shell", "input", "keyevent", "279"], 4.0)
                .await
                .map_err(|e| e.to_string())?;
        }
        if enter {
            let _ = state
                .adb
                .execute(&["-s", &serial, "shell", "input", "keyevent", "66"], 4.0)
                .await;
        }
    }
    Ok(())
}

#[tauri::command]
async fn toggle_screen_power(state: State<'_, Arc<AppState>>, on: bool) -> Result<(), String> {
    state.scrcpy.set_screen_power_mode(on).await
}

#[tauri::command]
async fn capture_screenshot_png(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let serial = state
        .active_serial
        .lock()
        .await
        .clone()
        .ok_or_else(|| "No active device selected".to_string())?;
    let png_bytes = state
        .adb
        .execute_bytes(&["-s", &serial, "exec-out", "screencap", "-p"], 5.0)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(dims) = parse_png_dimensions(&png_bytes) {
        *state.display_dimensions.lock().await = dims;
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(png_bytes))
}

#[tauri::command]
async fn toggle_mcp_http(
    state: State<'_, Arc<AppState>>,
    enable: bool,
    port: Option<u16>,
) -> Result<McpServerStatus, String> {
    if enable {
        let target_port = port.unwrap_or(8766);
        let bound = state
            .mcp_http
            .start(target_port, None)
            .await
            .map_err(|e| e.to_string())?;
        *state.mcp_http_port.lock().await = Some(bound);
        Ok(McpServerStatus {
            running: true,
            port: Some(bound),
            sse_url: Some(format!("http://127.0.0.1:{bound}/sse")),
        })
    } else {
        state.mcp_http.stop().await;
        *state.mcp_http_port.lock().await = None;
        Ok(McpServerStatus {
            running: false,
            port: None,
            sse_url: None,
        })
    }
}

#[tauri::command]
async fn get_mcp_status(state: State<'_, Arc<AppState>>) -> Result<McpServerStatus, String> {
    let p = *state.mcp_http_port.lock().await;
    Ok(McpServerStatus {
        running: p.is_some(),
        port: p,
        sse_url: p.map(|port| format!("http://127.0.0.1:{port}/sse")),
    })
}

async fn query_device_wm_size(adb: &DroidPathAdbRunner, serial: &str) -> Result<(u32, u32), String> {
    let out = adb
        .execute(&["-s", serial, "shell", "wm", "size"], 2.0)
        .await
        .map_err(|e| e.to_string())?;
    for line in out.lines().rev() {
        if let Some((_, dims)) = line.split_once(':') {
            let trimmed = dims.trim();
            if let Some((w, h)) = trimmed.split_once('x') {
                if let (Ok(wn), Ok(hn)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    return Ok((wn, hn));
                }
            }
        }
    }
    Err("Unable to parse wm size".to_string())
}

/// Captures or clears `pending_hitl` from `MacroExecutionEvent` streams so hazardous confirmations
/// triggered by external MCP agents (over embedded HTTP/SSE) can be Approved/Cancelled in the HUD.
pub(crate) async fn sync_pending_hitl_from_event(
    state: &AppState,
    ev: &MacroExecutionEvent,
) {
    match ev {
        MacroExecutionEvent::AwaitingConfirmation { request, .. } => {
            let mut guard = state.pending_hitl.lock().await;
            if guard.is_none() {
                let serial = match state.active_serial.lock().await.clone() {
                    Some(s) => Some(s),
                    None => state.manager.active_serial().await,
                };
                if let Some(serial) = serial {
                    *guard = Some(PendingHitlNavigation {
                        serial,
                        target: None,
                        target_hash: Some(request.target_hash),
                        max_steps: request.total_steps.clamp(1, 30),
                        step_timeout_ms: 2000,
                        request: request.clone(),
                    });
                }
            }
        }
        MacroExecutionEvent::Completed { .. } | MacroExecutionEvent::Failed { .. } => {
            *state.pending_hitl.lock().await = None;
        }
        _ => {}
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let adb = DroidPathAdbRunner::default();
            let storage = DroidPathPersistenceCoordinator::default_storage_path();
            let bundled_jar = app
                .path()
                .resource_dir()
                .ok()
                .map(|dir| dir.join("android-daemon.jar"))
                .filter(|p| p.is_file());
            let manager = DroidPathManager::new(27189, storage, bundled_jar, adb.clone(), None, None);
            let scrcpy = Arc::new(ScrcpyBridge::new(adb.clone()));
            let mcp_core = Arc::new(DroidPathMcpServerCore::new(Arc::clone(&manager)));
            let mcp_http = Arc::new(DroidPathHttpServer::new(mcp_core));

            let state = Arc::new(AppState {
                adb,
                manager: Arc::clone(&manager),
                scrcpy,
                mcp_http,
                mcp_http_port: Mutex::new(None),
                active_serial: Mutex::new(None),
                display_dimensions: Mutex::new((1080, 2400)),
                pending_hitl: Mutex::new(None),
            });

            app.manage(Arc::clone(&state));

            // Relay MacroExecutionEvent -> frontend Agentic HUD and capture MCP-triggered HITL state
            let handle_hud = app.handle().clone();
            let state_for_hud = Arc::clone(&state);
            let mut macro_rx = manager.bridge.subscribe_macro_events();
            tauri::async_runtime::spawn(async move {
                loop {
                    match macro_rx.recv().await {
                        Ok(ev) => {
                            sync_pending_hitl_from_event(&state_for_hud, &ev).await;
                            let _ = handle_hud.emit("hud-event", &ev);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            // Relay FLAG_SECURE / biometrics events -> frontend Agentic HUD
            let handle_sec = app.handle().clone();
            let mut sec_rx = manager.bridge.subscribe_secure_screen_events();
            tauri::async_runtime::spawn(async move {
                loop {
                    match sec_rx.recv().await {
                        Ok(payload) => {
                            let _ = handle_sec.emit("secure-screen", &payload);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            // Periodic graph stats & snapshot telemetry push
            let handle_stats = app.handle().clone();
            let mgr_stats = Arc::clone(&manager);
            tauri::async_runtime::spawn(async move {
                mgr_stats.load_persisted_graph_if_needed().await;
                let mut interval = tokio::time::interval(Duration::from_millis(1200));
                loop {
                    interval.tick().await;
                    let stats = DesktopGraphStatsDto::from(mgr_stats.graph_stats().await);
                    let _ = handle_stats.emit("graph-stats", &stats);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            connect_device,
            disconnect_device,
            get_graph_stats,
            get_graph_snapshot,
            get_ui_tree,
            navigate_to_node,
            resolve_hitl_confirmation,
            inject_touch,
            inject_swipe,
            inject_scroll,
            inject_keyevent,
            inject_text,
            toggle_screen_power,
            capture_screenshot_png,
            toggle_mcp_http,
            get_mcp_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while building DroidPath Desktop");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                let state = Arc::clone(&state);
                tauri::async_runtime::block_on(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(2), async {
                        state.scrcpy.stop().await;
                        state.mcp_http.stop().await;
                        state.manager.stop().await;
                    })
                    .await;
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidpath_core::{HazardCategory, ScreenStateVertex, ScreenTransitionEdge};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn test_orient_display_dimensions_handles_downscaled_portrait_and_landscape_rotation() {
        // Pixel 10a physical 1080x2424, scrcpy max_size=1280 -> video 576x1280 (portrait)
        assert_eq!(orient_display_dimensions((1080, 2424), (576, 1280)), (1080, 2424));
        // After rotating phone to landscape, video becomes 1280x576 -> physical flips to 2424x1080
        assert_eq!(orient_display_dimensions((1080, 2424), (1280, 576)), (2424, 1080));

        // Clicking 90%, 90% on downscaled 576x1280 video must resolve to physical ~972, 2182 (NOT 518, 1152!)
        let (px, py) = normalized_to_physical_display(0.9, 0.9, (1080, 2424), (576, 1280));
        assert_eq!((px, py), (972, 2182));

        // Landscape click at 90%, 90% resolves to 2182, 972
        let (lx, ly) = normalized_to_physical_display(0.9, 0.9, (1080, 2424), (1280, 576));
        assert_eq!((lx, ly), (2182, 972));

        // PNG IHDR parser extracts landscape screenshot dimensions accurately
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2424u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(parse_png_dimensions(&png), Some((2424, 1080)));
    }

    #[test]
    fn test_desktop_graph_snapshot_dto_preserves_high_bit_u64_hashes_as_hex() {
        let high_hash: u64 = 0xfedc_ba98_7654_3211;
        let target_hash: u64 = 0x8000_0000_0000_0001;
        let now = Utc::now();
        let snap = SpatialGraphSnapshot {
            root_state_hash: Some(high_hash),
            current_state_hash: Some(target_hash),
            vertices: vec![ScreenStateVertex {
                layout_hash: high_hash,
                active_package: "com.example.shop".to_string(),
                sample_interactive_labels: vec!["Cart".to_string()],
                semantic_tokens: BTreeSet::new(),
                first_seen_at: now,
                last_seen_at: now,
                visit_count: 2,
                discovery_parent_hash: None,
                discovery_depth: 0,
            }],
            edges: vec![ScreenTransitionEdge {
                id: uuid::Uuid::new_v4(),
                from_hash: high_hash,
                to_hash: target_hash,
                action: None,
                action_signature: "tap:Pay".to_string(),
                traversal_count: 1,
                average_transition_ms: 120.0,
                is_spanning_tree_edge: true,
                last_traversed_at: now,
                is_hazardous: true,
                hazard_reason: Some("payment".to_string()),
            }],
            spanning_children: BTreeMap::new(),
        };

        let dto = DesktopGraphSnapshotDto::from(snap);
        assert_eq!(dto.root_state_hash_hex.as_deref(), Some("0xfedcba9876543211"));
        assert_eq!(dto.current_state_hash_hex.as_deref(), Some("0x8000000000000001"));
        assert_eq!(dto.vertices[0].layout_hash_hex, "0xfedcba9876543211");
        assert_eq!(dto.edges[0].from_hash_hex, "0xfedcba9876543211");
        assert_eq!(dto.edges[0].to_hash_hex, "0x8000000000000001");
    }

    #[tokio::test]
    async fn test_mcp_triggered_hitl_event_populates_and_clears_pending_state() {
        let adb = DroidPathAdbRunner::default();
        let tmp_graph = std::env::temp_dir().join(format!(
            "droidpath-desktop-hitl-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        let manager = DroidPathManager::new(27189, tmp_graph, None, adb.clone(), None, None);
        let scrcpy = Arc::new(ScrcpyBridge::new(adb.clone()));
        let mcp_core = Arc::new(DroidPathMcpServerCore::new(Arc::clone(&manager)));
        let mcp_http = Arc::new(DroidPathHttpServer::new(mcp_core));

        let state = AppState {
            adb,
            manager,
            scrcpy,
            mcp_http,
            mcp_http_port: Mutex::new(None),
            active_serial: Mutex::new(Some("emulator-5554".to_string())),
            display_dimensions: Mutex::new((1080, 2400)),
            pending_hitl: Mutex::new(None),
        };

        let req = HazardousConfirmationRequest {
            step_index: 1,
            total_steps: 3,
            action_signature: "tap:Pay $99".to_string(),
            target_description: "Pay $99".to_string(),
            hazard_category: HazardCategory::Payment,
            hazard_reason: "Payment confirmation".to_string(),
            current_hash: 0x1111,
            target_hash: 0x2222_3333_4444_5555,
            steps_completed: 1,
            tap_x: Some(540),
            tap_y: Some(1900),
        };

        sync_pending_hitl_from_event(
            &state,
            &MacroExecutionEvent::AwaitingConfirmation {
                step_index: 1,
                total_steps: 3,
                action: AttributedUIAction::tap("Pay $99", Some(540), Some(1900)),
                reason: "Payment confirmation".to_string(),
                request: req.clone(),
            },
        )
        .await;

        let captured = state.pending_hitl.lock().await.clone();
        assert!(captured.is_some());
        let nav = captured.unwrap();
        assert_eq!(nav.serial, "emulator-5554");
        assert_eq!(nav.target_hash, Some(0x2222_3333_4444_5555));
        assert_eq!(nav.request.action_signature, "tap:Pay $99");

        sync_pending_hitl_from_event(
            &state,
            &MacroExecutionEvent::Failed {
                message: "Cancelled".to_string(),
            },
        )
        .await;
        assert!(state.pending_hitl.lock().await.is_none());
    }
}
