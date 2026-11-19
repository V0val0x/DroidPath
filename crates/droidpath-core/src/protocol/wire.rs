use serde::{Deserialize, Deserializer, Serialize};

use super::node::ShadowUINode;

/// Bitmask matching `com.shadowgraph.daemon.graph.ShadowNode.PATCH_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShadowPatchMask(pub i32);

impl ShadowPatchMask {
    pub const PARENT: i32 = 1 << 0;
    pub const ORDER: i32 = 1 << 1;
    pub const TEXT: i32 = 1 << 2;
    pub const CONTENT_DESC: i32 = 1 << 3;
    pub const RESOURCE_ID: i32 = 1 << 4;
    pub const NODE_CLASS: i32 = 1 << 5;
    pub const BOUNDS: i32 = 1 << 6;
    pub const FLAGS: i32 = 1 << 7;

    #[inline]
    pub fn contains(self, flag: i32) -> bool {
        (self.0 & flag) != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireHelloPayload {
    pub protocol_version: i32,
    pub format: String,
    pub sdk_int: i32,
    pub device_model: String,
    pub pid: i64,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl WireHelloPayload {
    pub fn supports_native_input(&self) -> bool {
        self.capabilities.iter().any(|c| c == "input")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSnapshotPayload {
    pub seq: u64,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
    pub reason: String,
    #[serde(rename = "package")]
    pub active_package: String,
    #[serde(rename = "window_id")]
    pub active_window_id: i32,
    pub node_count: usize,
    pub nodes: Vec<ShadowUINode>,
    #[serde(rename = "secure_screen", default)]
    pub secure_screen_active: Option<bool>,
}

fn deserialize_double_option<'de, D>(deserializer: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::<i64>::deserialize(deserializer)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireNodePatch {
    pub node_id: i64,
    pub mask: i32,
    /// `None` => key omitted; `Some(None)` => explicit `"parent_id": null`; `Some(Some(id))` => explicit id.
    #[serde(
        rename = "parent_id",
        default,
        deserialize_with = "deserialize_double_option"
    )]
    pub parent_id_patch: Option<Option<i64>>,
    #[serde(default)]
    pub index: Option<i32>,
    #[serde(default)]
    pub child_index: Option<i32>,
    #[serde(default)]
    pub depth: Option<i32>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub content_desc: Option<String>,
    #[serde(default)]
    pub resource_id: Option<String>,
    #[serde(rename = "class", default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub package_name: Option<String>,
    #[serde(default)]
    pub bounds: Option<String>,
    #[serde(default)]
    pub center_x: Option<i32>,
    #[serde(default)]
    pub center_y: Option<i32>,
    #[serde(default)]
    pub clickable: Option<bool>,
    #[serde(default)]
    pub scrollable: Option<bool>,
    #[serde(default)]
    pub editable: Option<bool>,
    #[serde(default)]
    pub focused: Option<bool>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub visible: Option<bool>,
}

impl WireNodePatch {
    #[inline]
    pub fn has_parent_id_key(&self) -> bool {
        self.parent_id_patch.is_some()
    }

    #[inline]
    pub fn parent_id(&self) -> Option<i64> {
        self.parent_id_patch.flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireDiffPayload {
    pub base_seq: u64,
    pub seq: u64,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
    #[serde(rename = "package")]
    pub active_package: String,
    #[serde(rename = "window_id")]
    pub active_window_id: i32,
    #[serde(default)]
    pub event_type: Option<i32>,
    #[serde(default)]
    pub event_package: Option<String>,
    #[serde(default)]
    pub inserted: Vec<ShadowUINode>,
    #[serde(default)]
    pub updated: Vec<WireNodePatch>,
    #[serde(default)]
    pub removed: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireHeartbeatPayload {
    pub seq: u64,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
    pub uptime_ms: i64,
    #[serde(rename = "package")]
    pub active_package: String,
    #[serde(rename = "window_id")]
    pub active_window_id: i32,
    pub clients: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WirePongPayload {
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSecureScreenPayload {
    pub code: String,
    pub active: bool,
    pub reason: String,
    #[serde(rename = "package")]
    pub active_package: String,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireInputResultPayload {
    pub id: u64,
    pub cmd: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    pub elapsed_ms: i64,
    #[serde(rename = "ts_ms")]
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    Hello(WireHelloPayload),
    Snapshot(WireSnapshotPayload),
    Diff(WireDiffPayload),
    Heartbeat(WireHeartbeatPayload),
    Error(WireErrorPayload),
    Pong(WirePongPayload),
    SecureScreen(WireSecureScreenPayload),
    InputResult(WireInputResultPayload),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonCommand {
    Snapshot,
    Ping,
    SetDebounce {
        ms: i32,
    },
    Shutdown,
    Tap {
        id: u64,
        x: i32,
        y: i32,
    },
    Swipe {
        id: u64,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: i32,
        hold_ms: i32,
    },
    Keyevent {
        id: u64,
        keycode: i32,
    },
}
