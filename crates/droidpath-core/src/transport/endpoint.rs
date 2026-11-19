use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::WireHelloPayload;

#[derive(Debug, Clone, PartialEq)]
pub struct DaemonEndpointConfiguration {
    pub host: String,
    pub port: u16,
    pub initial_reconnect_delay_secs: f64,
    pub max_reconnect_delay_secs: f64,
    pub heartbeat_timeout_secs: f64,
    pub max_frame_bytes: usize,
}

impl DaemonEndpointConfiguration {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            initial_reconnect_delay_secs: 0.25,
            max_reconnect_delay_secs: 5.0,
            heartbeat_timeout_secs: 12.0,
            max_frame_bytes: 8 * 1024 * 1024,
        }
    }

    pub fn with_port(port: u16) -> Self {
        Self::new("127.0.0.1", port)
    }
}

impl Default for DaemonEndpointConfiguration {
    fn default() -> Self {
        Self::new("127.0.0.1", 27189)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonConnectionState {
    Disconnected,
    Connecting,
    Connected {
        hello: Option<WireHelloPayload>,
    },
    WaitingToReconnect {
        attempt: usize,
        delay_seconds: f64,
        last_error: Option<String>,
    },
    Stopped,
}

impl DaemonConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Stopped => "stopped".to_string(),
            Self::Connecting => "connecting".to_string(),
            Self::Connected { .. } => "connected".to_string(),
            Self::WaitingToReconnect { attempt, .. } => format!("waiting_reconnect_{attempt}"),
            Self::Disconnected => "disconnected".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonClientMetrics {
    pub frames_received: u64,
    pub bytes_received: u64,
    pub decode_errors: u64,
    pub reconnect_count: u64,
    pub last_frame_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DaemonClientError {
    #[error("Invalid daemon endpoint {host}:{port}")]
    InvalidEndpoint { host: String, port: u16 },
    #[error("Daemon TCP client is not connected")]
    NotConnected,
    #[error("Daemon TCP client is stopped")]
    Stopped,
    #[error("Failed to send command to daemon: {0}")]
    SendFailed(String),
}
