use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// ISO-8601 seconds formatter matching Swift's `JSONEncoder.DateEncodingStrategy.iso8601`
/// (`YYYY-MM-DDTHH:MM:SSZ` without fractional seconds) so `AndOnMac` decodes with 100% compatibility.
pub mod swift_iso8601 {
    use super::*;

    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = date.to_rfc3339_opts(SecondsFormat::Secs, true);
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(de::Error::custom)
    }
}

/// Uppercase UUID serializer matching Swift's `UUID.uuidString`.
pub mod swift_uuid {
    use super::*;

    pub fn serialize<S>(uuid: &Uuid, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        let upper = uuid.hyphenated().encode_upper(&mut buf);
        serializer.serialize_str(upper)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Uuid::parse_str(&s).map_err(de::Error::custom)
    }
}

/// Swift's `JSONEncoder` serializes `[UInt64: [UInt64]]` as an alternating JSON array:
/// `[key1, [child1, child2], key2, [child3]]`.
/// This codec serializes in Swift's exact alternating-array format (for 100% `AndOnMac`
/// compatibility) while deserializing BOTH alternating arrays AND JSON object maps `{"123": [...]}`.
pub mod swift_spanning_children {
    use super::*;

    pub fn serialize<S>(
        map: &BTreeMap<u64, Vec<u64>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(map.len() * 2))?;
        for (k, v) in map {
            seq.serialize_element(k)?;
            seq.serialize_element(v)?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BTreeMap<u64, Vec<u64>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SpanningVisitor;

        impl<'de> Visitor<'de> for SpanningVisitor {
            type Value = BTreeMap<u64, Vec<u64>>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an alternating array [key, [children...], ...] or map {key: [children...]}")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut out = BTreeMap::new();
                while let Some(key) = seq.next_element::<u64>()? {
                    let children = seq.next_element::<Vec<u64>>()?.ok_or_else(|| {
                        de::Error::custom("odd number of elements in spanningChildren array")
                    })?;
                    out.insert(key, children);
                }
                Ok(out)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = BTreeMap::new();
                while let Some((k_str, children)) = map.next_entry::<String, Vec<u64>>()? {
                    let key = k_str.parse::<u64>().map_err(de::Error::custom)?;
                    out.insert(key, children);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_any(SpanningVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UIActionKind {
    Tap,
    Swipe,
    Keyevent,
    TypeText,
    LaunchApp,
}

impl UIActionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::Swipe => "swipe",
            Self::Keyevent => "keyevent",
            Self::TypeText => "typeText",
            Self::LaunchApp => "launchApp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttributedUIAction {
    pub kind: UIActionKind,
    pub target_description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_enter: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector_resource_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector_content_desc: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selector_aliases: Vec<String>,
    #[serde(with = "swift_iso8601")]
    pub timestamp: DateTime<Utc>,
}

impl AttributedUIAction {
    pub fn tap(target_description: impl Into<String>, x: Option<i32>, y: Option<i32>) -> Self {
        Self {
            kind: UIActionKind::Tap,
            target_description: target_description.into(),
            x,
            y,
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: None,
            press_enter: None,
            selector_resource_id: None,
            selector_text: None,
            selector_content_desc: None,
            selector_aliases: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn swipe(
        target_description: impl Into<String>,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: Option<i32>,
    ) -> Self {
        Self {
            kind: UIActionKind::Swipe,
            target_description: target_description.into(),
            x: Some(x1),
            y: Some(y1),
            end_x: Some(x2),
            end_y: Some(y2),
            duration_ms,
            text_input: None,
            press_enter: None,
            selector_resource_id: None,
            selector_text: None,
            selector_content_desc: None,
            selector_aliases: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn keyevent(keycode: impl Into<String>) -> Self {
        Self {
            kind: UIActionKind::Keyevent,
            target_description: keycode.into(),
            x: None,
            y: None,
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: None,
            press_enter: None,
            selector_resource_id: None,
            selector_text: None,
            selector_content_desc: None,
            selector_aliases: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn type_text(text: impl Into<String>, press_enter: bool) -> Self {
        let raw = text.into();
        let desc: String = raw.chars().take(32).collect();
        Self {
            kind: UIActionKind::TypeText,
            target_description: desc,
            x: None,
            y: None,
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: Some(raw),
            press_enter: Some(press_enter),
            selector_resource_id: None,
            selector_text: None,
            selector_content_desc: None,
            selector_aliases: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn launch_app(package_name: impl Into<String>) -> Self {
        Self {
            kind: UIActionKind::LaunchApp,
            target_description: package_name.into(),
            x: None,
            y: None,
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: None,
            press_enter: None,
            selector_resource_id: None,
            selector_text: None,
            selector_content_desc: None,
            selector_aliases: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn signature(&self) -> String {
        if !self.target_description.is_empty() {
            return format!("{}:{}", self.kind.as_str(), self.target_description);
        }
        if let (Some(x), Some(y)) = (self.x, self.y) {
            return format!("{}:({x},{y})", self.kind.as_str());
        }
        self.kind.as_str().to_string()
    }

    /// Returns a PII-scrubbed copy safe to store in `SpatialGraphEngine` and serialize into `shadow_graph.json`.
    pub fn redacted_for_storage(&self) -> Self {
        use super::hasher::ScreenLayoutHasher;
        Self {
            kind: self.kind,
            target_description: ScreenLayoutHasher::redact_pii(&self.target_description),
            x: self.x,
            y: self.y,
            end_x: self.end_x,
            end_y: self.end_y,
            duration_ms: self.duration_ms,
            text_input: self
                .text_input
                .as_deref()
                .map(ScreenLayoutHasher::redact_pii),
            press_enter: self.press_enter,
            selector_resource_id: self.selector_resource_id.clone(),
            selector_text: self
                .selector_text
                .as_deref()
                .map(ScreenLayoutHasher::redact_pii),
            selector_content_desc: self
                .selector_content_desc
                .as_deref()
                .map(ScreenLayoutHasher::redact_pii),
            selector_aliases: self
                .selector_aliases
                .iter()
                .map(|a| ScreenLayoutHasher::redact_pii(a))
                .collect(),
            timestamp: self.timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenStateVertex {
    pub layout_hash: u64,
    pub active_package: String,
    pub sample_interactive_labels: Vec<String>,
    #[serde(default)]
    pub semantic_tokens: BTreeSet<String>,
    #[serde(with = "swift_iso8601")]
    pub first_seen_at: DateTime<Utc>,
    #[serde(with = "swift_iso8601")]
    pub last_seen_at: DateTime<Utc>,
    pub visit_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_parent_hash: Option<u64>,
    pub discovery_depth: i32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_seed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenTransitionEdge {
    #[serde(with = "swift_uuid")]
    pub id: Uuid,
    pub from_hash: u64,
    pub to_hash: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<AttributedUIAction>,
    pub action_signature: String,
    pub traversal_count: i32,
    pub average_transition_ms: f64,
    pub is_spanning_tree_edge: bool,
    #[serde(with = "swift_iso8601")]
    pub last_traversed_at: DateTime<Utc>,
    #[serde(default)]
    pub is_hazardous: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hazard_reason: Option<String>,
    #[serde(default)]
    pub consecutive_failures: i32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_seed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpatialGraphSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_state_hash: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_state_hash: Option<u64>,
    pub vertices: Vec<ScreenStateVertex>,
    pub edges: Vec<ScreenTransitionEdge>,
    #[serde(default, with = "swift_spanning_children")]
    pub spanning_children: BTreeMap<u64, Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedVertexMatch {
    pub vertex: ScreenStateVertex,
    pub score: i32,
}
