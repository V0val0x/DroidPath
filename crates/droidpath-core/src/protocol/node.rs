use serde::{Deserialize, Serialize};

/// Compact, token-efficient representation of a meaningful Android UI node.
/// JSON-compatible with `AndroidMCPKit`, `AndOnMac`, and `DroidPath` MCP tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowCompactNode {
    pub index: i32,
    pub text: String,
    pub resource_id: String,
    pub content_desc: String,
    #[serde(rename = "class")]
    pub class_name: String,
    pub clickable: bool,
    pub scrollable: bool,
    pub bounds: String,
    pub center_x: i32,
    pub center_y: i32,
}

impl From<&ShadowUINode> for ShadowCompactNode {
    fn from(node: &ShadowUINode) -> Self {
        Self {
            index: node.index,
            text: node.text.clone(),
            resource_id: node.resource_id.clone(),
            content_desc: node.content_desc.clone(),
            class_name: node.class_name.clone(),
            clickable: node.clickable,
            scrollable: node.scrollable,
            bounds: node.bounds.clone(),
            center_x: node.center_x,
            center_y: node.center_y,
        }
    }
}

/// Full Shadow DOM node decoded from `android-daemon` (`ndjson/1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowUINode {
    pub index: i32,
    pub text: String,
    pub resource_id: String,
    pub content_desc: String,
    #[serde(rename = "class")]
    pub class_name: String,
    pub clickable: bool,
    pub scrollable: bool,
    pub bounds: String,
    pub center_x: i32,
    pub center_y: i32,

    pub node_id: i64,
    #[serde(default)]
    pub parent_id: Option<i64>,
    pub window_id: i32,
    pub child_index: i32,
    pub depth: i32,
    pub package_name: String,
    pub editable: bool,
    pub focused: bool,
    pub checked: bool,
    pub enabled: bool,
    pub visible: bool,
}

impl ShadowUINode {
    #[inline]
    pub fn to_compact_node(&self) -> ShadowCompactNode {
        ShadowCompactNode::from(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowTreeMatchMode {
    Exact,
    ContainsCaseInsensitive,
    SuffixOrExactResourceId,
}
