use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::adb::DroidPathAdbRunner;
use crate::bridge::{ExploreOutcome, ShadowNavigationOutcome};
use crate::manager::DroidPathManager;
use crate::protocol::ShadowCompactNode;
use crate::router::{MacroActionExecutor, PathDeviationError};
use crate::spatial_graph::{AgenticFeedback, AgenticFeedbackBuilder, AttributedUIAction};

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
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    pub title: &'static str,
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub annotations: McpToolAnnotations,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpContentBlock {
    Text { text: String },
    Image { data: String, mime_type: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolResult {
    pub text: String,
    pub is_error: bool,
    pub extra_content: Vec<McpContentBlock>,
}

impl McpToolResult {
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            text: text.into(),
            is_error,
            extra_content: Vec::new(),
        }
    }

    pub fn text_with_image(
        text: impl Into<String>,
        image_base64: impl Into<String>,
        mime_type: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            text: text.into(),
            is_error,
            extra_content: vec![McpContentBlock::Image {
                data: image_base64.into(),
                mime_type: mime_type.into(),
            }],
        }
    }

    pub fn to_json_value(&self) -> Value {
        let mut blocks = Vec::with_capacity(1 + self.extra_content.len());
        blocks.push(json!({
            "type": "text",
            "text": self.text
        }));
        for block in &self.extra_content {
            match block {
                McpContentBlock::Text { text } => blocks.push(json!({
                    "type": "text",
                    "text": text
                })),
                McpContentBlock::Image { data, mime_type } => blocks.push(json!({
                    "type": "image",
                    "data": data,
                    "mimeType": mime_type
                })),
            }
        }
        json!({
            "content": blocks,
            "isError": self.is_error
        })
    }
}

pub struct DroidPathMcpServerCore {
    pub manager: Arc<DroidPathManager>,
    pub adb_runner: DroidPathAdbRunner,
}

impl DroidPathMcpServerCore {
    pub const PROTOCOL_VERSION: &'static str = "2025-03-26";
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
            "initialize" => {
                let requested = request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str);
                let protocol_version = match requested {
                    Some(v @ ("2024-11-05" | "2025-03-26" | "2025-06-18")) => v,
                    _ => Self::PROTOCOL_VERSION,
                };
                McpJsonRpcResponse::ok(
                    req_id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": {
                            "tools": { "listChanged": false }
                        },
                        "serverInfo": {
                            "name": "droidpath-mcp",
                            "version": Self::SERVER_VERSION
                        }
                    }),
                )
            }

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
            "description": "Optional ADB device serial from android_list_devices. Defaults to the first connected Android device."
        });

        vec![
            McpToolDefinition {
                name: "android_navigate_to_node",
                description: "Computes and executes a shortest multi-step tap/swipe route to an Android screen or UI element using the persisted navigation graph without intermediate LLM calls. Use for known multi-screen workflows on the first connected ADB device (or device_serial); set auto_explore to true to fall back to live discovery if no cached route exists, or use android_explore_and_click for single-screen searches and android_tap_element for an already-visible control. Blocks until arrival, pauses with requires_confirmation on hazardous steps unless confirmed=true, and returns JSON with status (arrived, already_at_target, no_route, target_not_found, secure_screen_active, or path_deviation error), screen hashes, and executed steps.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Target UI text, resource-id, Android package name, or screen title (e.g. 'Night Light', 'Display'). Provide at least one of target or target_hash."
                        },
                        "target_hash": {
                            "type": "string",
                            "description": "Exact target layout hash in '0x...' hexadecimal or unsigned integer format. Optional alternative when target text is omitted."
                        },
                        "max_steps": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 30,
                            "default": 12,
                            "description": "Maximum navigation hops allowed across the graph before aborting (clamped to 1-30). Default: 12."
                        },
                        "step_timeout_ms": {
                            "type": "integer",
                            "minimum": 250,
                            "maximum": 10000,
                            "default": 2000,
                            "description": "Blocking per-step UI settlement wait timeout in milliseconds while verifying screen transitions (clamped to 250-10000). Default: 2000."
                        },
                        "confirmed": {
                            "type": "boolean",
                            "default": false,
                            "description": "Must remain false unless re-invoking after explicit user confirmation in chat for a previously blocked hazardous action (payment, purchase, delete). Default: false."
                        },
                        "auto_explore": {
                            "type": "boolean",
                            "default": false,
                            "description": "When true, automatically falls back to live scrolling discovery and graph recording if no cached route reaches the target. Default: false."
                        },
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
                annotations: McpToolAnnotations {
                    title: "Navigate to Android Screen or Node",
                    read_only_hint: false,
                    destructive_hint: true,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
            },
            McpToolDefinition {
                name: "android_explore_and_click",
                description: "Finds and taps an Android UI element on the current screen (or via a 'Screen > Item' breadcrumb) using live accessibility scanning, bounded scrolling, and optional graph routing on the first connected ADB device or device_serial. Use when the target may require scrolling or is absent from the navigation graph; prefer android_navigate_to_node for learned multi-screen paths, android_tap_element for known coordinates or selectors, or android_get_ui_tree to inspect without acting. Blocks until completion, records new transitions to disk, gates hazardous actions behind requires_confirmation unless confirmed=true, and returns JSON with status (explored_and_clicked, arrived_via_graph, or top-20 PII-redacted candidates under needs_selection when no match meets threshold).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Non-empty target UI label, resource_id suffix, content_desc, or 'Screen > Item' breadcrumb path (e.g. 'Night Light', 'Display > Night Light')."
                        },
                        "max_scrolls": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 10,
                            "default": 3,
                            "description": "Maximum bounded list scrolls (down then up) during live search before returning candidates (clamped to 0-10). Default: 3."
                        },
                        "step_timeout_ms": {
                            "type": "integer",
                            "minimum": 250,
                            "maximum": 10000,
                            "default": 2000,
                            "description": "Blocking per-step UI settlement wait in milliseconds after scrolls or route steps (clamped to 250-10000). Default: 2000."
                        },
                        "confirmed": {
                            "type": "boolean",
                            "default": false,
                            "description": "Must remain false unless re-invoking after explicit user confirmation in chat for a blocked hazardous/irreversible element. Default: false."
                        },
                        "prefer_graph_route": {
                            "type": "boolean",
                            "default": true,
                            "description": "When true, tries known graph routes before performing live on-screen scrolling search. Default: true."
                        },
                        "device_serial": serial_prop
                    },
                    "required": ["target"]
                }),
                annotations: McpToolAnnotations {
                    title: "Discover and Click Android Element",
                    read_only_hint: false,
                    destructive_hint: true,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
            },
            McpToolDefinition {
                name: "android_get_ui_tree",
                description: "Reads visible Android UI nodes from the live accessibility stream (falling back to uiautomator XML dump if the stream is cold) on the first connected ADB device or device_serial. Use for read-only UI inspection without touching the screen or modifying the navigation graph; prefer android_graph_stats for learned graph telemetry, or android_explore_and_click to locate and tap in a single call. Supports opt-in hybrid UI Tree + Vision streaming via include_vision=true (off by default, strictly blocked on FLAG_SECURE/biometric screens) to attach a heavily compressed JPEG screenshot alongside the structured UI tree for multimodal agents. Returns a JSON array of nodes (or a hybrid object + MCP image block when include_vision=true).",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "include_vision": {
                            "type": "boolean",
                            "default": false,
                            "description": "Opt-in multimodal mode: when true, attaches a heavily compressed downscaled JPEG screenshot alongside the UI node tree as a second MCP image content block. Strictly disabled by default and automatically blocked on FLAG_SECURE / biometric screens."
                        },
                        "vision_max_width": {
                            "type": "integer",
                            "minimum": 160,
                            "maximum": 1080,
                            "default": 540,
                            "description": "Maximum pixel width for the downscaled JPEG screenshot when include_vision is true (clamped to 160-1080). Default: 540."
                        },
                        "vision_quality": {
                            "type": "integer",
                            "minimum": 10,
                            "maximum": 85,
                            "default": 50,
                            "description": "JPEG compression quality (10-85) when include_vision is true. Lower values minimize multimodal token and bandwidth overhead. Default: 50."
                        },
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
                annotations: McpToolAnnotations {
                    title: "Read Live Android UI Tree",
                    read_only_hint: true,
                    destructive_hint: false,
                    idempotent_hint: true,
                    open_world_hint: false,
                },
            },
            McpToolDefinition {
                name: "android_graph_stats",
                description: "Returns a read-only telemetry snapshot of the persisted Android UI navigation graph without sending input to any device. Use before android_navigate_to_node to verify whether routes exist, inspect the active screen hash, or check accessibility daemon status; use android_get_ui_tree instead to inspect live UI elements on screen. Takes no arguments, executes locally without requiring an active device (reporting zero counts if no graph is learned yet), and returns JSON with vertex_count, seed_vertex_count, hydrated_vertex_count, edge_count, current_state_hash (0x hex or null), current_package, daemon_state, has_fresh_shadow_dom, and storage_path.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
                annotations: McpToolAnnotations {
                    title: "Get Spatial Graph Telemetry",
                    read_only_hint: true,
                    destructive_hint: false,
                    idempotent_hint: true,
                    open_world_hint: false,
                },
            },
            McpToolDefinition {
                name: "android_list_devices",
                description: "Lists connected Android devices visible to ADB over USB or Wi-Fi without altering device state. Use in multi-device setups to obtain exact device_serial values for other Android tools; unnecessary when only one device is attached because action tools default to the first device. Runs quickly in read-only mode and returns a JSON array of objects with serial, model, and connection ('usb' or 'wifi'), or an error if ADB is unavailable.",
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
                annotations: McpToolAnnotations {
                    title: "List Connected Android Devices",
                    read_only_hint: true,
                    destructive_hint: false,
                    idempotent_hint: true,
                    open_world_hint: false,
                },
            },
            McpToolDefinition {
                name: "android_tap_element",
                description: "Taps the first connected Android device (or device_serial) at exact screen pixel coordinates (x, y from top-left (0, 0); takes precedence if paired with selectors) or by matching a visible element via text, resource_id, or content_desc. Use when coordinates or exact selectors on the current screen are already known; prefer android_explore_and_click for off-screen scrolling or fuzzy matching, and android_swipe for dragging/scrolling. Records the tap in the persisted navigation graph, blocks briefly during selector lookup and input injection, and returns confirmation text with tapped coordinates or an error if inputs are missing or no node matches.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "x": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Horizontal tap position in screen pixels from the left edge (top-left origin 0, 0). Requires y; takes precedence over text/id/desc selectors."
                        },
                        "y": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Vertical tap position in screen pixels from the top edge (top-left origin 0, 0). Requires x; takes precedence over text/id/desc selectors."
                        },
                        "text": {
                            "type": "string",
                            "description": "Case-insensitive text match against visible UI nodes (combined with resource_id/content_desc when multiple selectors are provided)."
                        },
                        "resource_id": {
                            "type": "string",
                            "description": "Substring/suffix match against view resource-id (e.g. 'com.android.settings:id/search_bar')."
                        },
                        "content_desc": {
                            "type": "string",
                            "description": "Case-insensitive substring match against accessibility content-desc."
                        },
                        "device_serial": serial_prop
                    },
                    "required": []
                }),
                annotations: McpToolAnnotations {
                    title: "Tap Android Element or Coordinates",
                    read_only_hint: false,
                    destructive_hint: true,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
            },
            McpToolDefinition {
                name: "android_swipe",
                description: "Performs a blocking touch swipe from start (x1, y1) to end (x2, y2) in screen pixel coordinates (top-left origin (0, 0)) over duration_ms on the first connected ADB device or device_serial, and records the swipe edge in the navigation graph. Use for custom coordinate-based scrolling, dismissing panels, or dragging when start and end pixels are known; prefer android_explore_and_click to scroll and click a named list item automatically, android_tap_element for single taps, and android_navigate_to_node for learned paths. Blocks until the swipe finishes and returns confirmation text with coordinates and effective duration in ms, or an error if ADB fails.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "x1": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Swipe start X coordinate in screen pixels from the left edge (>= 0)."
                        },
                        "y1": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Swipe start Y coordinate in screen pixels from the top edge (>= 0)."
                        },
                        "x2": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Swipe end X coordinate in screen pixels from the left edge (>= 0)."
                        },
                        "y2": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Swipe end Y coordinate in screen pixels from the top edge (>= 0)."
                        },
                        "duration_ms": {
                            "type": "integer",
                            "minimum": 50,
                            "maximum": 5000,
                            "default": 300,
                            "description": "Optional swipe duration in milliseconds (clamped to 50-5000). When omitted, uses zero-fling daemon timing or 300 ms on ADB shell fallback."
                        },
                        "device_serial": serial_prop
                    },
                    "required": ["x1", "y1", "x2", "y2"]
                }),
                annotations: McpToolAnnotations {
                    title: "Perform Android Touch Swipe",
                    read_only_hint: false,
                    destructive_hint: false,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
            },
            McpToolDefinition {
                name: "android_type_text",
                description: "Types ASCII or Unicode text into the currently focused Android input field on the first connected ADB device (or device_serial), optionally sending ENTER afterward, and records the input action in the navigation graph. Focus the field first with android_tap_element or android_explore_and_click if no cursor is active; use android_keyevent instead for navigation or hardware keys. Blocks until text entry (via input command or clipboard paste for non-ASCII) completes and returns the typed character count, or an error if text is empty or ADB dispatch fails.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Non-empty ASCII or Unicode string to type into the currently focused field."
                        },
                        "press_enter": {
                            "type": "boolean",
                            "default": false,
                            "description": "When true, sends Android ENTER (keycode 66) immediately after typing. Default: false."
                        },
                        "device_serial": serial_prop
                    },
                    "required": ["text"]
                }),
                annotations: McpToolAnnotations {
                    title: "Type Text into Focused Android Field",
                    read_only_hint: false,
                    destructive_hint: false,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
            },
            McpToolDefinition {
                name: "android_keyevent",
                description: "Sends a system or hardware Android keyevent (such as HOME, BACK, ENTER, APP_SWITCH, POWER, VOLUME_UP, or an integer KeyEvent code) to the first connected ADB device (or device_serial) and records the action in the navigation graph. Use for system navigation, dismissing dialogs (BACK), toggling screen sleep (POWER), or hardware keys instead of coordinate/selector UI tools (android_tap_element, android_swipe, android_type_text); note POWER alters device lock state. Blocks until dispatched and returns the key name and numeric code, or an error on unrecognized key names or ADB failure.",
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "keycode": {
                            "type": "string",
                            "description": "Case-insensitive Android key name (optional 'KEYCODE_' prefix: HOME, BACK, RECENTS, APP_SWITCH, ENTER, DELETE, VOLUME_UP, VOLUME_DOWN, POWER, WAKEUP, NOTIFICATION, PASTE) or unsigned integer Android KeyEvent code (e.g. '3', '4', '26')."
                        },
                        "device_serial": serial_prop
                    },
                    "required": ["keycode"]
                }),
                annotations: McpToolAnnotations {
                    title: "Send Android Hardware/System Keyevent",
                    read_only_hint: false,
                    destructive_hint: true,
                    idempotent_hint: false,
                    open_world_hint: true,
                },
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
        let get_serial = || get_str("device_serial").or_else(|| get_str("serial"));
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
                    "seed_vertex_count": stats.seed_vertex_count,
                    "hydrated_vertex_count": stats.hydrated_vertex_count,
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
                let include_vision = get_bool("include_vision").unwrap_or(false);
                let vision_max_width = get_i32("vision_max_width").unwrap_or(540).clamp(160, 1080);
                let vision_quality = get_i32("vision_quality").unwrap_or(50).clamp(10, 85);

                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_serial())
                    .await
                    .map_err(|e| e.to_string())?;
                self.manager.ensure_started(Some(&serial), false).await;

                let mut nodes_opt = self.manager.fast_compact_tree_if_fresh(0.8, 15.0).await;
                if nodes_opt.is_none() {
                    let port = self.manager.bridge.client.configuration().port;
                    if port > 0
                        && self
                            .manager
                            .recover_daemon_and_await_stream(&serial, port)
                            .await
                    {
                        nodes_opt = self.manager.fast_compact_tree_if_fresh(0.8, 15.0).await;
                    }
                }
                let nodes = match nodes_opt {
                    Some(cached) => cached,
                    None => {
                        let xml = self.fetch_fallback_ui_xml(&serial).await?;
                        DroidPathUITreeFallbackParser::parse(&xml)
                    }
                };

                if !include_vision {
                    let s = serde_json::to_string(&nodes).map_err(|e| e.to_string())?;
                    return Ok(McpToolResult::text(s, false));
                }

                // Opt-in hybrid mode: strictly block vision capture when FLAG_SECURE is active.
                if self.manager.bridge.dom_state.is_secure_screen_active().await {
                    let payload = json!({
                        "nodes": nodes,
                        "vision": {
                            "included": false,
                            "blocked_reason": "FLAG_SECURE",
                            "message": "Vision frame strictly blocked because FLAG_SECURE / biometric screen is active."
                        }
                    });
                    return Ok(McpToolResult::text(payload.to_string(), false));
                }

                let _ = self
                    .manager
                    .bridge
                    .client
                    .wait_for_hello(std::time::Duration::from_millis(150))
                    .await;

                match self
                    .manager
                    .bridge
                    .client
                    .request_vision_frame(
                        vision_max_width,
                        vision_quality,
                        std::time::Duration::from_secs(3),
                    )
                    .await
                {
                    Ok(frame)
                        if frame.ok
                            && frame
                                .data_base64
                                .as_deref()
                                .is_some_and(|s| !s.is_empty()) =>
                    {
                        let b64 = frame.data_base64.unwrap_or_default();
                        let payload = json!({
                            "nodes": nodes,
                            "vision": {
                                "included": true,
                                "mime_type": frame.mime_type,
                                "width": frame.width,
                                "height": frame.height,
                                "quality": frame.quality,
                                "byte_length": frame.byte_length,
                                "elapsed_ms": frame.elapsed_ms
                            }
                        });
                        Ok(McpToolResult::text_with_image(
                            payload.to_string(),
                            b64,
                            frame.mime_type,
                            false,
                        ))
                    }
                    Ok(frame) => {
                        let payload = json!({
                            "nodes": nodes,
                            "vision": {
                                "included": false,
                                "blocked_reason": frame.blocked_reason,
                                "error": frame.error
                            }
                        });
                        Ok(McpToolResult::text(payload.to_string(), false))
                    }
                    Err(err) => {
                        let payload = json!({
                            "nodes": nodes,
                            "vision": {
                                "included": false,
                                "error": err.to_string()
                            }
                        });
                        Ok(McpToolResult::text(payload.to_string(), false))
                    }
                }
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
                    .resolve_target_serial(get_serial())
                    .await
                    .map_err(|e| e.to_string())?;
                let max_steps = get_i32("max_steps").unwrap_or(12).clamp(1, 30) as usize;
                let step_timeout_ms =
                    get_i32("step_timeout_ms").unwrap_or(2000).clamp(250, 10_000) as u64;
                let confirmed = get_bool("confirmed").unwrap_or(false);
                let auto_explore = get_bool("auto_explore").unwrap_or(false);

                let pre_index = self.manager.bridge.dom_state.current_index().await;
                let target_for_fb = target.clone();
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
                    Ok(outcome) => {
                        let mut res = Self::format_navigation_outcome(&outcome);
                        let fb_code = match &outcome {
                            ShadowNavigationOutcome::NoRoute { .. } => Some("no_route"),
                            ShadowNavigationOutcome::TargetNotFound { .. } => {
                                Some("target_not_found")
                            }
                            ShadowNavigationOutcome::Explored(
                                ExploreOutcome::NeedsSelection { .. },
                            ) => Some("element_not_found"),
                            _ => None,
                        };
                        if let Some(code) = fb_code {
                            let fb = self
                                .build_live_feedback(
                                    code,
                                    target_for_fb.as_deref(),
                                    pre_index.as_deref(),
                                )
                                .await;
                            res = Self::attach_feedback_to_result(res, &fb);
                        }
                        Ok(res)
                    }
                    Err(deviation) => {
                        let res = Self::format_path_deviation_result(&deviation);
                        let fb = self
                            .build_live_feedback(
                                "route_deviation",
                                target_for_fb.as_deref(),
                                pre_index.as_deref(),
                            )
                            .await;
                        Ok(Self::attach_feedback_to_result(res, &fb))
                    }
                }
            }

            "android_explore_and_click" => {
                let Some(target) = get_str("target").map(str::trim).filter(|s| !s.is_empty()) else {
                    return Ok(McpToolResult::text("Missing required 'target' parameter.", true));
                };
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_serial())
                    .await
                    .map_err(|e| e.to_string())?;
                let max_scrolls = get_i32("max_scrolls").unwrap_or(3).clamp(0, 10) as usize;
                let step_timeout_ms =
                    get_i32("step_timeout_ms").unwrap_or(2000).clamp(250, 10_000) as u64;
                let confirmed = get_bool("confirmed").unwrap_or(false);
                let prefer_graph_route = get_bool("prefer_graph_route").unwrap_or(true);
                let pre_index = self.manager.bridge.dom_state.current_index().await;

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
                    Ok(outcome) => {
                        let mut res = Self::format_explore_outcome(&outcome);
                        if matches!(outcome, ExploreOutcome::NeedsSelection { .. }) {
                            let fb = self
                                .build_live_feedback(
                                    "element_not_found",
                                    Some(target),
                                    pre_index.as_deref(),
                                )
                                .await;
                            res = Self::attach_feedback_to_result(res, &fb);
                        }
                        Ok(res)
                    }
                    Err(deviation) => {
                        let res = Self::format_path_deviation_result(&deviation);
                        let fb = self
                            .build_live_feedback(
                                "route_deviation",
                                Some(target),
                                pre_index.as_deref(),
                            )
                            .await;
                        Ok(Self::attach_feedback_to_result(res, &fb))
                    }
                }
            }

            "android_tap_element" => {
                let serial = self
                    .adb_runner
                    .resolve_target_serial(get_serial())
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
                let pre_index = self.manager.bridge.dom_state.current_index().await;
                let mut shadow_ready = self.manager.bridge.ensure_shadow_stream_ready(0.8, 15.0).await;
                if !shadow_ready {
                    let port = self.manager.bridge.client.configuration().port;
                    if port > 0
                        && self
                            .manager
                            .recover_daemon_and_await_stream(&serial, port)
                            .await
                    {
                        shadow_ready = self.manager.bridge.dom_state.is_fresh(15.0).await;
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

                // Only fall back to `uiautomator dump` when the Shadow DOM stream is cold/unavailable;
                // invoking `uiautomator` while the daemon holds UiAutomation fails with exit 137 on Android
                // and would abort before structured `element_not_found` feedback is emitted.
                if resolved.is_none() && !shadow_ready {
                    let xml = self.fetch_fallback_ui_xml(&serial).await?;
                    let nodes = DroidPathUITreeFallbackParser::parse(&xml);
                    resolved = Self::find_matching_compact(&nodes, text_q, id_q, desc_q);
                }

                let Some(m) = resolved else {
                    let query = text_q.or(desc_q).or(id_q);
                    let fb = self
                        .build_live_feedback("element_not_found", query, pre_index.as_deref())
                        .await;
                    let payload = json!({
                        "status": "element_not_found",
                        "message": "No matching UI element found.",
                        "feedback": fb
                    });
                    return Ok(McpToolResult::text(payload.to_string(), true));
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
                    .resolve_target_serial(get_serial())
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
                    .resolve_target_serial(get_serial())
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
                let Some(raw) = get_str("keycode")
                    .or_else(|| get_str("key"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
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
                    .resolve_target_serial(get_serial())
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

    async fn build_live_feedback(
        &self,
        failure_code: &str,
        requested_target: Option<&str>,
        previous_index: Option<&crate::shadow_dom::ShadowTreeIndex>,
    ) -> AgenticFeedback {
        let current_index = self.manager.bridge.dom_state.current_index().await;
        let is_secure = self.manager.bridge.dom_state.is_secure_screen_active().await;
        AgenticFeedbackBuilder::build(
            failure_code,
            requested_target,
            previous_index,
            current_index.as_deref(),
            is_secure,
        )
    }

    fn attach_feedback_to_result(mut result: McpToolResult, feedback: &AgenticFeedback) -> McpToolResult {
        if let Ok(mut val) = serde_json::from_str::<Value>(&result.text) {
            if let Some(obj) = val.as_object_mut() {
                if let Ok(fb_val) = serde_json::to_value(feedback) {
                    obj.insert("feedback".to_string(), fb_val);
                    result.text = val.to_string();
                }
            }
        }
        result
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
