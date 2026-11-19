use serde::Deserialize;
use thiserror::Error;

use super::wire::{
    DaemonCommand, WireDiffPayload, WireErrorPayload, WireHeartbeatPayload, WireHelloPayload,
    WireInputResultPayload, WireMessage, WirePongPayload, WireSecureScreenPayload,
    WireSnapshotPayload,
};

#[derive(Debug, Error)]
pub enum WireCodecError {
    #[error("Empty wire frame")]
    EmptyFrame,
    #[error("Unknown message type '{0}'")]
    UnknownMessageType(String),
    #[error("Invalid debounce ms: {0} (expected 0..=5000)")]
    InvalidDebounceMs(i32),
    #[error("Invalid input parameter: {0}")]
    InvalidInputParameter(String),
    #[error("JSON decode error: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait WireCodec: Send + Sync {
    fn format_name(&self) -> &'static str;
    fn decode_frame(&self, frame_data: &[u8]) -> Result<WireMessage, WireCodecError>;
    fn encode_command(&self, command: DaemonCommand) -> Result<Vec<u8>, WireCodecError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NdjsonWireCodec;

#[derive(Deserialize)]
struct TypeHeader<'a> {
    #[serde(rename = "type")]
    msg_type: &'a str,
}

impl NdjsonWireCodec {
    const CMD_SNAPSHOT: &'static [u8] = b"{\"cmd\":\"snapshot\"}\n";
    const CMD_PING: &'static [u8] = b"{\"cmd\":\"ping\"}\n";
    const CMD_SHUTDOWN: &'static [u8] = b"{\"cmd\":\"shutdown\"}\n";

    const PREFIX_DIFF: &'static [u8] = br#"{"type":"diff""#;
    const PREFIX_SNAPSHOT: &'static [u8] = br#"{"type":"snapshot""#;
    const PREFIX_HEARTBEAT: &'static [u8] = br#"{"type":"heartbeat""#;
    const PREFIX_PONG: &'static [u8] = br#"{"type":"pong""#;
    const PREFIX_HELLO: &'static [u8] = br#"{"type":"hello""#;
    const PREFIX_ERROR: &'static [u8] = br#"{"type":"error""#;
    const PREFIX_SECURE_SCREEN: &'static [u8] = br#"{"type":"secure_screen""#;
    const PREFIX_INPUT_RESULT: &'static [u8] = br#"{"type":"input_result""#;

    pub fn new() -> Self {
        Self
    }

    fn matches_prefix(data: &[u8], prefix: &[u8]) -> bool {
        if data.len() < prefix.len() || &data[..prefix.len()] != prefix {
            return false;
        }
        if data.len() > prefix.len() {
            let next = data[prefix.len()];
            return matches!(next, b',' | b'}' | b' ' | b'\t');
        }
        true
    }

    fn fast_detect_message_type(data: &[u8]) -> Option<&'static str> {
        if data.len() < 15 {
            return None;
        }
        if Self::matches_prefix(data, Self::PREFIX_DIFF) {
            return Some("diff");
        }
        if Self::matches_prefix(data, Self::PREFIX_SNAPSHOT) {
            return Some("snapshot");
        }
        if Self::matches_prefix(data, Self::PREFIX_HEARTBEAT) {
            return Some("heartbeat");
        }
        if Self::matches_prefix(data, Self::PREFIX_PONG) {
            return Some("pong");
        }
        if Self::matches_prefix(data, Self::PREFIX_HELLO) {
            return Some("hello");
        }
        if Self::matches_prefix(data, Self::PREFIX_ERROR) {
            return Some("error");
        }
        if Self::matches_prefix(data, Self::PREFIX_SECURE_SCREEN) {
            return Some("secure_screen");
        }
        if Self::matches_prefix(data, Self::PREFIX_INPUT_RESULT) {
            return Some("input_result");
        }
        None
    }

    fn decode_payload(msg_type: &str, data: &[u8]) -> Result<WireMessage, WireCodecError> {
        match msg_type {
            "diff" => Ok(WireMessage::Diff(serde_json::from_slice::<WireDiffPayload>(
                data,
            )?)),
            "snapshot" => Ok(WireMessage::Snapshot(serde_json::from_slice::<
                WireSnapshotPayload,
            >(data)?)),
            "heartbeat" => Ok(WireMessage::Heartbeat(serde_json::from_slice::<
                WireHeartbeatPayload,
            >(data)?)),
            "pong" => Ok(WireMessage::Pong(serde_json::from_slice::<WirePongPayload>(
                data,
            )?)),
            "hello" => Ok(WireMessage::Hello(
                serde_json::from_slice::<WireHelloPayload>(data)?,
            )),
            "error" => Ok(WireMessage::Error(
                serde_json::from_slice::<WireErrorPayload>(data)?,
            )),
            "secure_screen" => Ok(WireMessage::SecureScreen(serde_json::from_slice::<
                WireSecureScreenPayload,
            >(data)?)),
            "input_result" => Ok(WireMessage::InputResult(serde_json::from_slice::<
                WireInputResultPayload,
            >(data)?)),
            other => Err(WireCodecError::UnknownMessageType(other.to_string())),
        }
    }
}

impl WireCodec for NdjsonWireCodec {
    fn format_name(&self) -> &'static str {
        "ndjson/1"
    }

    fn decode_frame(&self, frame_data: &[u8]) -> Result<WireMessage, WireCodecError> {
        if frame_data.is_empty() {
            return Err(WireCodecError::EmptyFrame);
        }
        if let Some(fast_type) = Self::fast_detect_message_type(frame_data) {
            return Self::decode_payload(fast_type, frame_data);
        }
        let header: TypeHeader<'_> = serde_json::from_slice(frame_data)?;
        Self::decode_payload(header.msg_type, frame_data)
    }

    fn encode_command(&self, command: DaemonCommand) -> Result<Vec<u8>, WireCodecError> {
        match command {
            DaemonCommand::Snapshot => Ok(Self::CMD_SNAPSHOT.to_vec()),
            DaemonCommand::Ping => Ok(Self::CMD_PING.to_vec()),
            DaemonCommand::Shutdown => Ok(Self::CMD_SHUTDOWN.to_vec()),
            DaemonCommand::SetDebounce { ms } => {
                if !(0..=5000).contains(&ms) {
                    return Err(WireCodecError::InvalidDebounceMs(ms));
                }
                Ok(format!("{{\"cmd\":\"set_debounce\",\"ms\":{ms}}}\n").into_bytes())
            }
            DaemonCommand::Tap { id, x, y } => {
                if !(0..=16_384).contains(&x) || !(0..=16_384).contains(&y) {
                    return Err(WireCodecError::InvalidInputParameter(format!(
                        "tap coords ({x},{y}) out of range 0..=16384"
                    )));
                }
                Ok(format!("{{\"cmd\":\"tap\",\"id\":{id},\"x\":{x},\"y\":{y}}}\n").into_bytes())
            }
            DaemonCommand::Swipe {
                id,
                x1,
                y1,
                x2,
                y2,
                duration_ms,
                hold_ms,
            } => {
                if [x1, y1, x2, y2].iter().any(|c| !(0..=16_384).contains(c)) {
                    return Err(WireCodecError::InvalidInputParameter(format!(
                        "swipe coords ({x1},{y1})->({x2},{y2}) out of range 0..=16384"
                    )));
                }
                if !(10..=10_000).contains(&duration_ms) {
                    return Err(WireCodecError::InvalidInputParameter(format!(
                        "swipe duration_ms {duration_ms} out of range 10..=10000"
                    )));
                }
                if !(0..=1_000).contains(&hold_ms) {
                    return Err(WireCodecError::InvalidInputParameter(format!(
                        "swipe hold_ms {hold_ms} out of range 0..=1000"
                    )));
                }
                Ok(format!(
                    "{{\"cmd\":\"swipe\",\"id\":{id},\"x1\":{x1},\"y1\":{y1},\"x2\":{x2},\"y2\":{y2},\"duration_ms\":{duration_ms},\"hold_ms\":{hold_ms}}}\n"
                )
                .into_bytes())
            }
            DaemonCommand::Keyevent { id, keycode } => {
                if !(1..=1024).contains(&keycode) {
                    return Err(WireCodecError::InvalidInputParameter(format!(
                        "keycode {keycode} out of range 1..=1024"
                    )));
                }
                Ok(
                    format!("{{\"cmd\":\"keyevent\",\"id\":{id},\"keycode\":{keycode}}}\n")
                        .into_bytes(),
                )
            }
        }
    }
}
