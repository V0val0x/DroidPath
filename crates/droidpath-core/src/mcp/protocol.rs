use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::adb::DroidPathAdbRunner;
use crate::bridge::{ExploreOutcome, ShadowNavigationOutcome};
use crate::manager::DroidPathManager;
use crate::protocol::ShadowCompactNode;
use crate::router::{MacroActionExecutor, PathDeviationError};
use crate::spatial_graph::AttributedUIAction;

use super::xml_fallback::DroidPathUITreeFallbackParser;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpJsonRpcRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpJsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpJsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpJsonRpcError>,
}

impl McpJsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(McpJsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    pub text: String,
    pub is_error: bool,
}

impl McpToolResult {
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            text: text.into(),
            is_error,
        }
    }

    pub fn to_json_value(&self) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": self.text
            }],
            "isError": self.is_error
        })
    }
}

pub struct DroidPathMcpServerCore {
    pub manager: Arc<DroidPathManager>,
    pub adb_runner: DroidPathAdbRunner,
}

impl DroidPathMcpServerCore {
    pub const PROTOCOL_VERSION: &'static str = "2024-11-05";
    pub const SERVER_VERSION: &'static str = env!("CARGO_PKG_VERSION");

    pub fn new(manager: Arc<DroidPathManager>) -> Self {
        let adb_runner = manager.adb_runner().clone();
        Self {
            manager,
            adb_runner,
        }
    }

    pub async fn handle_raw_request_data(&self, data: &[u8]) -> Option<Vec<u8>> {
        let request: McpJsonRpcRequest = match serde_json::from_slice(data) {
            Ok(req) => req,
            Err(err) => {
                let resp = McpJsonRpcResponse::err(
                    Value::Null,
                    -32700,
                    format!("Parse error: {err}"),
                );
                return serde_json::to_vec(&resp).ok();
            }
        };

        // JSON-RPC notifications have no `id`
        let req_id = request.id?;

        let response = match request.method.as_str() {
            "initialize" => McpJsonRpcResponse::ok(
                req_id,
                json!({
                    "protocolVersion": Self::PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": "droidpath-mcp",
                        "version": Self::SERVER_VERSION
                    }
                }),
            ),

            "ping" => McpJsonRpcResponse::ok(req_id, json!({})),

            "tools/list" => {
                let defs = Self::tool_definitions();
                McpJsonRpcResponse::ok(req_id, json!({ "tools": defs }))
            }

            "tools/call" => {
                let name = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("arguments"))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();

                if name.is_empty() {
                    McpJsonRpcResponse::err(req_id, -32602, "Missing tool name in params.name")
                } else {
                    let result = self.execute_tool(name, &arguments).await;
                    McpJsonRpcResponse::ok(req_id, result.to_json_value())
                }
            }

            other => McpJsonRpcResponse::err(req_id, -32601, format!("Method not found: {other}")),
        };

        serde_json::to_vec(&response).ok()
    }

    pub fn tool_definitions() -> Vec<McpToolDefinition> {
        let serial_prop = json!({
            "type": "string",
            "description": "Optional ADB device serial. Defaults to the first connected device."
        });

        vec![
            McpToolDefinition {
                name: "android_navigate_to_node",
                description: "Layer 3 Persistent A* Symbolic UI Router. Computes the shortest path across known screens in the Spatial Graph and executes taps/swipes via live Shadow DOM feedback without intermediate LLM calls.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Target UI text, resource-id, package name, or screen title (e.g. 'Night Light', 'Display and touch')."
                        },
                        "target_hash": {
                            "type": "string",
                            "description": "Optional exact target layout hash in 0x... hex or decimal format."
                        },
                        "max_steps": {
                            "type": "integer",
                            "description": "Maximum navigation hops allowed (1-30). Default: 12."
                        },
                        "step_timeout_ms": {
                            "type": "integer",
                            "description": "Per-step Shadow DOM settlement timeout in ms (250-10000). Default: 2000."
                        },
                        "confirmed": {
                            "type": "boolean",
                            "description": "Set to true ONLY after explicit human confirmation in chat to execute hazardous/irreversible steps (payment, purchase, delete). Default: false."
                        },
                        "auto_explore": {
                            "type": "boolean",
                            "description": "When true and no known graph route exists (no_route / target_not_found), automatically fall back to live Shadow DOM explore-and-click with bounded scrolling and DAG edge recording. Default: false."
                        },
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
            },
            McpToolDefinition {
                name: "android_explore_and_click",
                description: "Dynamic Shadow DOM discovery + click with optional A* graph/prefix routing, bounded list scrolling (down/up), hazardous action HITL guard, and automatic Spatial Graph edge recording. Returns top-20 PII-redacted interactive candidates (needs_selection, isError: false) if no confident match is found.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Target UI label, resource_id suffix, content_desc, or breadcrumb path (e.g. 'Night Light', 'Display > Night Light')."
                        },
                        "max_scrolls": {
                            "type": "integer",
                            "description": "Maximum bounded discovery scrolls (0-10). Default: 3."
                        },
                        "step_timeout_ms": {
                            "type": "integer",
                            "description": "Per-step Shadow DOM settlement timeout in ms (250-10000). Default: 2000."
                        },
                        "confirmed": {
                            "type": "boolean",
                            "description": "Set to true ONLY after explicit human confirmation in chat to click hazardous/irreversible elements. Default: false."
                        },
                        "prefer_graph_route": {
                            "type": "boolean",
                            "description": "Try known A* graph route (or nearest known prefix vertex) before scanning live Shadow DOM. Default: true."
                        },
                        "device_serial": serial_prop
                    },
                    "required": ["target"]
                }),
            },
            McpToolDefinition {
                name: "android_get_ui_tree",
                description: "Reads interactive Android UI elements from the sub-millisecond Kotlin Shadow DOM stream (falls back to uiautomator XML if daemon stream is cold).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
            },
            McpToolDefinition {
                name: "android_graph_stats",
                description: "Returns Spatial Graph telemetry: learned screen vertices, action edges, active canonical layout hash, daemon state, and persisted graph path.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            McpToolDefinition {
                name: "android_list_devices",
                description: "Lists connected Android devices visible over USB or Wireless ADB.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            McpToolDefinition {
                name: "android_tap_element",
                description: "Taps an Android UI element by coordinates (x, y) or semantic text / resource_id / content_desc, recording the transition edge into the Spatial Graph.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "x": { "type": "integer", "description": "Exact screen X coordinate." },
                        "y": { "type": "integer", "description": "Exact screen Y coordinate." },
                        "text": { "type": "string", "description": "Case-insensitive substring match against node text." },
                        "resource_id": { "type": "string", "description": "Match against view resource-id." },
                        "content_desc": { "type": "string", "description": "Substring match against content-desc." },
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
            },
            McpToolDefinition {
                name: "android_swipe",
                description: "Performs a touch swipe from (x1, y1) to (x2, y2) and attributes the action to the Spatial Graph.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "x1": { "type": "integer" },
                        "y1": { "type": "integer" },
                        "x2": { "type": "integer" },
                        "y2": { "type": "integer" },
                        "duration_ms": { "type": "integer", "description": "Swipe duration in ms (default 300)." },
                        "device_serial": serial_prop
                    },
                    "required": ["x1", "y1", "x2", "y2"]
                }),
            },
            McpToolDefinition {
                name: "android_type_text",
                description: "Types ASCII or Unicode text into the focused Android input field with optional press_enter.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to type." },
                        "press_enter": { "type": "boolean", "description": "Send ENTER keyevent after typing." },
                        "device_serial": serial_prop
                    },
                    "required": ["text"]
                }),
            },
            McpToolDefinition {
                name: "android_keyevent",
                description: "Sends an Android keyevent (e.g. HOME, BACK, ENTER, APP_SWITCH, POWER).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "keycode": { "type": "string", "description": "Key name or integer keycode." },
                        "device_serial": serial_prop
                    },
                    "required": ["keycode"]
                }),
            },
        ]
    }

    pub async fn execute_tool(&self, name: &str, arguments: &Map<String, Value>) -> McpToolResult {
        match self.execute_tool_inner(name, arguments).await {
            Ok(res) => res,
            Err(err) => McpToolResult::text(format!("Tool '{name}' error: {err}"), true),
        }
    }

    async fn execute_tool_inner(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<McpToolResult, String> {
        let get_str = |k: &str| arguments.get(k).and_then(Value::as_str);
        let get_i32 = |k: &str| {
            arguments
                .get(k)
                .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                .map(|n| n as i32)
        };
        let get_bool = |k: &str| arguments.get(k).and_then(Value::as_bool);

        match name {
            "android_graph_stats" => {
                let stats = self.manager.graph_stats().await;
                let val = json!({
                    "vertex_count": stats.vertex_count,
                    "edge_count": stats.edge_count,
                    "current_state_hash": stats.current_state_hash.map(Self::hex),
                    "current_package": stats.current_package,
                    "daemon_state": stats.connection_state.describe(),
                    "has_fresh_shadow_dom": stats.has_fresh_shadow_dom,
                    "storage_path": stats.storage_path
                });
                Ok(McpToolResult::text(val.to_string(), false))
            }

            "android_list_devices" => {
                let devices = self
                    .adb_runner
                    .list_devices()
                    .await
                    .map_err(|e| e.to_string())?;
                let rows: Vec<Value> = devices
                    .into_iter()
                    .map(|d| {
                        json!({
                            "serial": d.serial,
                            "model": d.model,
                            "connection": if d.is_wireless { "wifi" } else { "usb" }
                        })
                    })
                    .collect();
                Ok(McpToolResult::text(Value::Array(rows).to_string(), false))
            }

            "android_get_ui_tree" => {
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                self.manager.ensure_started(Some(&serial), false).await;
                if let Some(cached) = self.manager.fast_compact_tree_if_fresh(0.8, 15.0).await {
                    let s = serde_json::to_string(&cached).map_err(|e| e.to_string())?;
                    return Ok(McpToolResult::text(s, false));
                }
                let port = self.manager.bridge.client.configuration().port;
                if port > 0
                    && self
                        .manager
                        .recover_daemon_and_await_stream(&serial, port)
                        .await
                {
                    if let Some(cached) = self.manager.fast_compact_tree_if_fresh(0.8, 15.0).await {
                        let s = serde_json::to_string(&cached).map_err(|e| e.to_string())?;
                        return Ok(McpToolResult::text(s, false));
                    }
                }
                let xml = self.fetch_fallback_ui_xml(&serial).await?;
                let parsed = DroidPathUITreeFallbackParser::parse(&xml);
                let s = serde_json::to_string(&parsed).map_err(|e| e.to_string())?;
                Ok(McpToolResult::text(s, false))
            }

            "android_navigate_to_node" => {
                let target = get_str("target")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string);
                let target_hash = Self::parse_hash(arguments.get("target_hash"));

                if target.is_none() && target_hash.is_none() {
                    return Ok(McpToolResult::text(
                        "Provide either 'target' or 'target_hash'.",
                        true,
                    ));
                }

                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                let max_steps = get_i32("max_steps").unwrap_or(12).clamp(1, 30) as usize;
                let step_timeout_ms =
                    get_i32("step_timeout_ms").unwrap_or(2000).clamp(250, 10_000) as u64;
                let confirmed = get_bool("confirmed").unwrap_or(false);
                let auto_explore = get_bool("auto_explore").unwrap_or(false);

                match self
                    .manager
                    .navigate_to_node(
                        &serial,
                        target,
                        target_hash,
                        max_steps,
                        step_timeout_ms,
                        confirmed,
                        auto_explore,
                        60.0,
                        None,
                    )
                    .await
                {
                    Ok(outcome) => Ok(Self::format_navigation_outcome(&outcome)),
                    Err(deviation) => Ok(Self::format_path_deviation_result(&deviation)),
                }
            }

            "android_explore_and_click" => {
                let Some(target) = get_str("target").map(str::trim).filter(|s| !s.is_empty()) else {
                    return Ok(McpToolResult::text("Missing required 'target' parameter.", true));
                };
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                let max_scrolls = get_i32("max_scrolls").unwrap_or(3).clamp(0, 10) as usize;
                let step_timeout_ms =
                    get_i32("step_timeout_ms").unwrap_or(2000).clamp(250, 10_000) as u64;
                let confirmed = get_bool("confirmed").unwrap_or(false);
                let prefer_graph_route = get_bool("prefer_graph_route").unwrap_or(true);

                match self
                    .manager
                    .explore_and_click(
                        &serial,
                        target,
                        max_scrolls,
                        step_timeout_ms,
                        confirmed,
                        prefer_graph_route,
                        60.0,
                        None,
                    )
                    .await
                {
                    Ok(outcome) => Ok(Self::format_explore_outcome(&outcome)),
                    Err(deviation) => Ok(Self::format_path_deviation_result(&deviation)),
                }
            }

            "android_tap_element" => {
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                if let (Some(x), Some(y)) = (get_i32("x"), get_i32("y")) {
                    self.manager.ensure_started(Some(&serial), false).await;
                    let _ = self
                        .manager
                        .bridge
                        .client
                        .wait_for_hello(std::time::Duration::from_millis(150))
                        .await;
                    let _ = self
                        .manager
                        .bridge
                        .ensure_shadow_stream_ready(0.5, 15.0)
                        .await;
                    let attributed = if self.manager.bridge.dom_state.is_fresh(15.0).await {
                        if let Some(idx) = self.manager.bridge.dom_state.current_index().await {
                            idx.attributed_tap_action(x, y, chrono::Utc::now())
                        } else {
                            AttributedUIAction::tap(format!("({x},{y})"), Some(x), Some(y))
                        }
                    } else {
                        AttributedUIAction::tap(format!("({x},{y})"), Some(x), Some(y))
                    };
                    self.manager.record_action(attributed).await;
                    self.manager
                        .action_executor()
                        .dispatch_tap(&serial, x, y)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(McpToolResult::text(format!("Tapped at ({x}, {y})."), false));
                }

                let text_q = get_str("text");
                let id_q = get_str("resource_id");
                let desc_q = get_str("content_desc");
                if text_q.is_none() && id_q.is_none() && desc_q.is_none() {
                    return Ok(McpToolResult::text(
                        "Provide either (x, y) or one of text, resource_id, content_desc.",
                        true,
                    ));
                }

                self.manager.ensure_started(Some(&serial), false).await;
                if !self.manager.bridge.ensure_shadow_stream_ready(0.8, 15.0).await {
                    let port = self.manager.bridge.client.configuration().port;
                    if port > 0 {
                        let _ = self
                            .manager
                            .recover_daemon_and_await_stream(&serial, port)
                            .await;
                    }
                }
                let mut resolved: Option<ShadowCompactNode> = None;

                if let (Some(t), None, None) = (text_q, id_q, desc_q) {
                    if let Some(n) = self.manager.find_node_by_text(t, true).await {
                        resolved = Some(n.to_compact_node());
                    }
                } else if let (None, Some(rid), None) = (text_q, id_q, desc_q) {
                    if let Some(n) = self.manager.find_node_by_resource_id(rid).await {
                        resolved = Some(n.to_compact_node());
                    }
                } else if let (None, None, Some(desc)) = (text_q, id_q, desc_q) {
                    if let Some(n) = self.manager.find_node_by_content_desc(desc).await {
                        resolved = Some(n.to_compact_node());
                    }
                } else if let Some(live_nodes) =
                    self.manager.fast_compact_tree_if_fresh(0.25, 15.0).await
                {
                    resolved = Self::find_matching_compact(&live_nodes, text_q, id_q, desc_q);
                }

                if resolved.is_none() {
                    let xml = self.fetch_fallback_ui_xml(&serial).await?;
                    let nodes = DroidPathUITreeFallbackParser::parse(&xml);
                    resolved = Self::find_matching_compact(&nodes, text_q, id_q, desc_q);
                }

                let Some(m) = resolved else {
                    return Ok(McpToolResult::text("No matching UI element found.", true));
                };

                let target_desc = if !m.text.is_empty() {
                    m.text.clone()
                } else if !m.content_desc.is_empty() {
                    m.content_desc.clone()
                } else {
                    m.resource_id.clone()
                };

                let mut act =
                    AttributedUIAction::tap(target_desc, Some(m.center_x), Some(m.center_y));
                act.selector_resource_id = (!m.resource_id.is_empty()).then(|| m.resource_id.clone());
                act.selector_text = (!m.text.is_empty()).then(|| m.text.clone());
                act.selector_content_desc =
                    (!m.content_desc.is_empty()).then(|| m.content_desc.clone());
                self.manager.record_action(act).await;

                self.manager
                    .action_executor()
                    .dispatch_tap(&serial, m.center_x, m.center_y)
                    .await
                    .map_err(|e| e.to_string())?;
                let display_name = if !m.text.is_empty() {
                    &m.text
                } else {
                    &m.resource_id
                };
                Ok(McpToolResult::text(
                    format!("Tapped '{display_name}' at ({}, {}).", m.center_x, m.center_y),
                    false,
                ))
            }

            "android_swipe" => {
                let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                    (get_i32("x1"), get_i32("y1"), get_i32("x2"), get_i32("y2"))
                else {
                    return Ok(McpToolResult::text(
                        "Missing required swipe coordinates (x1, y1, x2, y2).",
                        true,
                    ));
                };
                let duration_opt = get_i32("duration_ms").map(|d| d.clamp(50, 5000));
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                self.manager.ensure_started(Some(&serial), false).await;
                let _ = self
                    .manager
                    .bridge
                    .client
                    .wait_for_hello(std::time::Duration::from_millis(150))
                    .await;
                let executor = self.manager.action_executor();
                let effective_dur = duration_opt.unwrap_or(
                    if executor.supports_zero_fling_swipes(&serial).await {
                        crate::adb::DroidPathActionExecutor::DEFAULT_DAEMON_SWIPE_DURATION_MS
                    } else {
                        300
                    },
                );
                self.manager
                    .record_action(AttributedUIAction::swipe(
                        format!("swipe ({x1},{y1})->({x2},{y2})"),
                        x1,
                        y1,
                        x2,
                        y2,
                        Some(effective_dur),
                    ))
                    .await;
                executor
                    .dispatch_swipe(
                        &serial,
                        x1,
                        y1,
                        x2,
                        y2,
                        Some(effective_dur),
                        crate::adb::DroidPathActionExecutor::DEFAULT_ZERO_FLING_HOLD_MS,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(McpToolResult::text(
                    format!("Swiped ({x1},{y1}) -> ({x2},{y2}) in {effective_dur}ms."),
                    false,
                ))
            }

            "android_type_text" => {
                let Some(text) = get_str("text").filter(|s| !s.is_empty()) else {
                    return Ok(McpToolResult::text("Missing 'text' parameter.", true));
                };
                let press_enter = get_bool("press_enter").unwrap_or(false);
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                self.manager.ensure_started(Some(&serial), false).await;
                self.manager
                    .record_action(AttributedUIAction::type_text(text, press_enter))
                    .await;
                let (use_clipboard, quoted) =
                    DroidPathAdbRunner::build_safe_type_text_command(text);
                if !use_clipboard {
                    self.adb_runner
                        .execute(&["-s", &serial, "shell", "input", "text", &quoted], 5.0)
                        .await
                        .map_err(|e| e.to_string())?;
                } else {
                    self.adb_runner
                        .execute(
                            &["-s", &serial, "shell", "cmd", "clipboard", "set", "text", &quoted],
                            4.0,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    self.manager
                        .action_executor()
                        .dispatch_keyevent_code(&serial, "279")
                        .await
                        .map_err(|e| e.to_string())?;
                }
                if press_enter {
                    self.manager
                        .action_executor()
                        .dispatch_keyevent_code(&serial, "66")
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Ok(McpToolResult::text(
                    format!(
                        "Typed {} characters{}.",
                        text.chars().count(),
                        if press_enter { " + ENTER" } else { "" }
                    ),
                    false,
                ))
            }

            "android_keyevent" => {
                let Some(raw) = get_str("keycode").map(str::trim).filter(|s| !s.is_empty()) else {
                    return Ok(McpToolResult::text("Missing 'keycode' parameter.", true));
                };
                let Some(key_code) = DroidPathAdbRunner::resolve_key_code(raw) else {
                    return Ok(McpToolResult::text(
                        format!("Unsupported or invalid keyevent '{raw}'."),
                        true,
                    ));
                };
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_str("device_serial"))
                    .await
                    .map_err(|e| e.to_string())?;
                self.manager.ensure_started(Some(&serial), false).await;
                let _ = self
                    .manager
                    .bridge
                    .client
                    .wait_for_hello(std::time::Duration::from_millis(150))
                    .await;
                self.manager
                    .record_action(AttributedUIAction::keyevent(raw.to_uppercase()))
                    .await;
                self.manager
                    .action_executor()
                    .dispatch_keyevent_code(&serial, &key_code)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(McpToolResult::text(
                    format!("Sent keyevent {} ({key_code}).", raw.to_uppercase()),
                    false,
                ))
            }

            other => Ok(McpToolResult::text(format!("Unknown tool '{other}'."), true)),
        }
    }

    fn find_matching_compact(
        nodes: &[ShadowCompactNode],
        text_q: Option<&str>,
        id_q: Option<&str>,
        desc_q: Option<&str>,
    ) -> Option<ShadowCompactNode> {
        let t_low = text_q.map(str::to_lowercase);
        let id_low = id_q.map(str::to_lowercase);
        let d_low = desc_q.map(str::to_lowercase);
        nodes
            .iter()
            .find(|node| {
                if let Some(ref t) = t_low {
                    if !node.text.to_lowercase().contains(t) {
                        return false;
                    }
                }
                if let Some(ref rid) = id_low {
                    if !node.resource_id.to_lowercase().contains(rid) {
                        return false;
                    }
                }
                if let Some(ref d) = d_low {
                    if !node.content_desc.to_lowercase().contains(d) {
                        return false;
                    }
                }
                true
            })
            .cloned()
    }

    async fn fetch_fallback_ui_xml(&self, serial: &str) -> Result<String, String> {
        let mut xml = self
            .adb_runner
            .execute(
                &["-s", serial, "exec-out", "uiautomator", "dump", "/dev/tty"],
                8.0,
            )
            .await
            .unwrap_or_default();
        if !xml.contains("<hierarchy") {
            let remote_dump = "/sdcard/droidpath_dump.xml";
            self.adb_runner
                .execute(&["-s", serial, "shell", "uiautomator", "dump", remote_dump], 8.0)
                .await
                .map_err(|e| e.to_string())?;
            xml = self
                .adb_runner
                .execute(&["-s", serial, "shell", "cat", remote_dump], 6.0)
                .await
                .map_err(|e| e.to_string())?;
            let _ = self
                .adb_runner
                .execute(&["-s", serial, "shell", "rm", "-f", remote_dump], 3.0)
                .await;
        }
        Ok(xml)
    }

    pub fn hex(hash: u64) -> String {
        format!("0x{hash:016x}")
    }

    pub fn parse_hash(val: Option<&Value>) -> Option<u64> {
        let v = val?;
        if let Some(u) = v.as_u64() {
            return Some(u);
        }
        if let Some(i) = v.as_i64() {
            return (i >= 0).then_some(i as u64);
        }
        let s = v.as_str()?.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(hex_str) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            return u64::from_str_radix(hex_str, 16).ok();
        }
        s.parse::<u64>()
            .ok()
            .or_else(|| u64::from_str_radix(s, 16).ok())
    }

    pub fn format_navigation_outcome(outcome: &ShadowNavigationOutcome) -> McpToolResult {
        match outcome {
            ShadowNavigationOutcome::Arrived(report) => {
                let summary: Vec<Value> = report
                    .steps
                    .iter()
                    .map(|step| {
                        json!({
                            "step": step.step_index,
                            "step_index": step.step_index,
                            "action": step.action_signature,
                            "target_hash": Self::hex(step.target_hash),
                            "elapsed_ms": step.elapsed_ms,
                            "matched_by_exact_hash": step.matched_by_exact_hash,
                            "skipped": step.was_skipped
                        })
                    })
                    .collect();
                let mut payload = json!({
                    "status": "arrived",
                    "start_hash": Self::hex(report.start_hash),
                    "target_hash": Self::hex(report.final_hash),
                    "steps_executed": report.steps.len(),
                    "total_elapsed_ms": report.total_elapsed_ms,
                    "path_summary": summary
                });
                if !report.recovered_steps.is_empty() {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "recovered_steps".to_string(),
                            json!(report.recovered_steps),
                        );
                    }
                }
                McpToolResult::text(payload.to_string(), false)
            }

            ShadowNavigationOutcome::AlreadyAtTarget {
                state_hash,
                active_package,
            } => McpToolResult::text(
                json!({
                    "status": "already_at_target",
                    "current_hash": Self::hex(*state_hash),
                    "active_package": active_package,
                    "steps_executed": 0,
                    "total_elapsed_ms": 0
                })
                .to_string(),
                false,
            ),

            ShadowNavigationOutcome::Explored(explore_outcome) => {
                Self::format_explore_outcome(explore_outcome)
            }

            ShadowNavigationOutcome::RequiresConfirmation(req) => McpToolResult::text(
                json!({
                    "status": "requires_confirmation",
                    "is_hazardous": true,
                    "hazard_category": req.hazard_category.as_str(),
                    "hazard_reason": req.hazard_reason,
                    "blocked_step_index": req.step_index,
                    "total_steps": req.total_steps,
                    "steps_completed": req.steps_completed,
                    "blocked_action": req.action_signature,
                    "target_description": req.target_description,
                    "current_hash": Self::hex(req.current_hash),
                    "target_hash": Self::hex(req.target_hash),
                    "message": "Irreversible action blocked pending human approval. Ask the user in chat to confirm, then re-run android_navigate_to_node with confirmed: true."
                })
                .to_string(),
                false,
            ),

            ShadowNavigationOutcome::SecureScreenTimeout {
                reason,
                active_package,
                timeout_seconds,
            } => McpToolResult::text(
                json!({
                    "status": "secure_screen_active",
                    "code": "SECURE_SCREEN_ACTIVE",
                    "reason": reason,
                    "active_package": active_package,
                    "timeout_seconds": timeout_seconds,
                    "message": format!("Protected biometric/PIN screen remained locked past {timeout_seconds}s. Ask the user to unlock on the phone and retry.")
                })
                .to_string(),
                true,
            ),

            ShadowNavigationOutcome::NoRoute {
                start_hash,
                active_package,
                candidate_target_hashes,
                reachable_vertex_count,
                known_vertex_count,
            } => McpToolResult::text(
                json!({
                    "status": "no_route",
                    "current_hash": Self::hex(*start_hash),
                    "active_package": active_package,
                    "candidate_target_hashes": candidate_target_hashes.iter().copied().map(Self::hex).collect::<Vec<_>>(),
                    "reachable_vertex_count": reachable_vertex_count,
                    "known_vertex_count": known_vertex_count
                })
                .to_string(),
                true,
            ),

            ShadowNavigationOutcome::TargetNotFound {
                query,
                known_vertex_count,
            } => McpToolResult::text(
                json!({
                    "status": "target_not_found",
                    "query": query,
                    "known_vertex_count": known_vertex_count
                })
                .to_string(),
                true,
            ),

            ShadowNavigationOutcome::StreamUnavailable => McpToolResult::text(
                json!({
                    "status": "stream_unavailable",
                    "message": "Shadow DOM stream is unavailable; ensure android-daemon is running and connected."
                })
                .to_string(),
                true,
            ),
        }
    }

    pub fn format_explore_outcome(outcome: &ExploreOutcome) -> McpToolResult {
        match outcome {
            ExploreOutcome::ArrivedViaGraph {
                report,
                already_at_target,
                active_package,
            } => {
                let summary: Vec<Value> = report
                    .steps
                    .iter()
                    .map(|step| {
                        json!({
                            "step_index": step.step_index,
                            "action": step.action_signature,
                            "target_hash": Self::hex(step.target_hash),
                            "elapsed_ms": step.elapsed_ms,
                            "matched_by_exact_hash": step.matched_by_exact_hash,
                            "skipped": step.was_skipped
                        })
                    })
                    .collect();
                let mut payload = json!({
                    "status": "arrived_via_graph",
                    "already_at_target": already_at_target,
                    "active_package": active_package,
                    "start_hash": Self::hex(report.start_hash),
                    "target_hash": Self::hex(report.final_hash),
                    "steps_executed": report.steps.len(),
                    "total_elapsed_ms": report.total_elapsed_ms,
                    "path_summary": summary
                });
                if !report.recovered_steps.is_empty() {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "recovered_steps".to_string(),
                            json!(report.recovered_steps),
                        );
                    }
                }
                McpToolResult::text(payload.to_string(), false)
            }

            ExploreOutcome::ExploredAndClicked {
                matched_candidate,
                scrolls_performed,
                start_hash,
                final_hash,
                active_package,
                screen_changed,
                edge_recorded,
                total_elapsed_ms,
            } => McpToolResult::text(
                json!({
                    "status": "explored_and_clicked",
                    "matched_candidate": matched_candidate,
                    "scrolls_performed": scrolls_performed,
                    "start_hash": Self::hex(*start_hash),
                    "final_hash": Self::hex(*final_hash),
                    "active_package": active_package,
                    "screen_changed": screen_changed,
                    "edge_recorded": edge_recorded,
                    "total_elapsed_ms": total_elapsed_ms
                })
                .to_string(),
                false,
            ),

            ExploreOutcome::RequiresConfirmation(req) => McpToolResult::text(
                json!({
                    "status": "requires_confirmation",
                    "is_hazardous": true,
                    "hazard_category": req.hazard_category.as_str(),
                    "hazard_reason": req.hazard_reason,
                    "blocked_step_index": req.step_index,
                    "total_steps": req.total_steps,
                    "steps_completed": req.steps_completed,
                    "blocked_action": req.action_signature,
                    "target_description": req.target_description,
                    "current_hash": Self::hex(req.current_hash),
                    "target_hash": Self::hex(req.target_hash),
                    "message": "Irreversible action blocked pending human approval. Ask the user in chat to confirm, then re-run android_explore_and_click with confirmed: true."
                })
                .to_string(),
                false,
            ),

            ExploreOutcome::SecureScreenTimeout {
                reason,
                active_package,
                timeout_seconds,
            } => McpToolResult::text(
                json!({
                    "status": "secure_screen_active",
                    "code": "SECURE_SCREEN_ACTIVE",
                    "reason": reason,
                    "active_package": active_package,
                    "timeout_seconds": timeout_seconds,
                    "message": format!("Protected biometric/PIN screen remained locked past {timeout_seconds}s. Ask the user to unlock on the phone and retry.")
                })
                .to_string(),
                true,
            ),

            ExploreOutcome::NeedsSelection {
                query,
                current_hash,
                active_package,
                scrolls_performed,
                candidates,
            } => McpToolResult::text(
                json!({
                    "status": "needs_selection",
                    "query": query,
                    "current_hash": Self::hex(*current_hash),
                    "active_package": active_package,
                    "scrolls_performed": scrolls_performed,
                    "candidate_count": candidates.len(),
                    "candidates": candidates,
                    "message": "No confident live match (>=500) found. Select a candidate from 'candidates' or refine 'target'."
                })
                .to_string(),
                false,
            ),
        }
    }

    pub fn format_path_deviation_result(error: &PathDeviationError) -> McpToolResult {
        let mut obj = Map::new();
        obj.insert("status".to_string(), json!("path_deviation"));
        obj.insert("message".to_string(), json!(error.to_string()));
        match error {
            PathDeviationError::ExecutionAlreadyInProgress => {
                obj.insert("error_type".to_string(), json!("execution_already_in_progress"));
            }
            PathDeviationError::ShadowStreamUnavailable => {
                obj.insert("error_type".to_string(), json!("shadow_stream_unavailable"));
            }
            PathDeviationError::MissingActionPayload {
                step_index,
                edge_id,
            } => {
                obj.insert("error_type".to_string(), json!("missing_action_payload"));
                obj.insert("step_index".to_string(), json!(step_index));
                obj.insert("edge_id".to_string(), json!(edge_id.to_string().to_uppercase()));
            }
            PathDeviationError::UnexpectedStartState {
                expected_hash,
                actual_hash,
                actual_package,
            } => {
                obj.insert("error_type".to_string(), json!("unexpected_start_state"));
                obj.insert("expected_hash".to_string(), json!(Self::hex(*expected_hash)));
                obj.insert(
                    "last_observed_hash".to_string(),
                    actual_hash.map(Self::hex).map(Value::String).unwrap_or(Value::Null),
                );
                obj.insert("last_observed_package".to_string(), json!(actual_package));
            }
            PathDeviationError::ActionDispatchFailed {
                step_index,
                action_signature,
                underlying_message,
            } => {
                obj.insert("error_type".to_string(), json!("action_dispatch_failed"));
                obj.insert("step_index".to_string(), json!(step_index));
                obj.insert("action".to_string(), json!(action_signature));
                obj.insert("underlying_error".to_string(), json!(underlying_message));
            }
            PathDeviationError::StepTimeout {
                step_index,
                expected_target_hash,
                last_observed_hash,
                last_observed_package,
                timeout_ms,
            } => {
                obj.insert("error_type".to_string(), json!("step_timeout"));
                obj.insert("step_index".to_string(), json!(step_index));
                obj.insert(
                    "expected_target_hash".to_string(),
                    json!(Self::hex(*expected_target_hash)),
                );
                obj.insert(
                    "last_observed_hash".to_string(),
                    last_observed_hash
                        .map(Self::hex)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                obj.insert(
                    "last_observed_package".to_string(),
                    json!(last_observed_package),
                );
                obj.insert("timeout_ms".to_string(), json!(timeout_ms));
            }
            PathDeviationError::RequiresConfirmation(req) => {
                obj.insert("status".to_string(), json!("requires_confirmation"));
                obj.insert("error_type".to_string(), json!("requires_confirmation"));
                obj.insert("hazard_category".to_string(), json!(req.hazard_category.as_str()));
                obj.insert("hazard_reason".to_string(), json!(req.hazard_reason));
                obj.insert("blocked_action".to_string(), json!(req.action_signature));
                obj.insert("blocked_step_index".to_string(), json!(req.step_index));
            }
            PathDeviationError::SecureScreenTimeout {
                reason,
                active_package,
                timeout_seconds,
            } => {
                obj.insert("status".to_string(), json!("secure_screen_active"));
                obj.insert("error_type".to_string(), json!("secure_screen_timeout"));
                obj.insert("reason".to_string(), json!(reason));
                obj.insert("active_package".to_string(), json!(active_package));
                obj.insert("timeout_seconds".to_string(), json!(timeout_seconds));
            }
        }
        McpToolResult::text(Value::Object(obj).to_string(), true)
    }
}
