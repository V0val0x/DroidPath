use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::spatial_graph::{AttributedUIAction, HazardCategory};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HazardousConfirmationRequest {
    pub step_index: usize,
    pub total_steps: usize,
    pub action_signature: String,
    pub target_description: String,
    pub hazard_category: HazardCategory,
    pub hazard_reason: String,
    pub current_hash: u64,
    pub target_hash: u64,
    pub steps_completed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_y: Option<i32>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PathDeviationError {
    #[error("A macro execution is already in progress.")]
    ExecutionAlreadyInProgress,
    #[error("Shadow DOM state is unavailable; cannot verify live screen.")]
    ShadowStreamUnavailable,
    #[error("Transition edge {edge_id} at step {step_index} has no executable UI action.")]
    MissingActionPayload { step_index: usize, edge_id: Uuid },
    #[error("Unexpected start state: expected 0x{expected_hash:016x}, observed {actual_hex} (package: {actual_package}).", actual_hex = format_opt_hash(*actual_hash))]
    UnexpectedStartState {
        expected_hash: u64,
        actual_hash: Option<u64>,
        actual_package: String,
    },
    #[error("Failed to dispatch '{action_signature}' at step {step_index}: {underlying_message}")]
    ActionDispatchFailed {
        step_index: i32,
        action_signature: String,
        underlying_message: String,
    },
    #[error("Path deviation at step {step_index}: ShadowDOM did not settle into 0x{expected_target_hash:016x} within {timeout_ms}ms (last observed: {actual_hex}, package: {last_observed_package}).", actual_hex = format_opt_hash(*last_observed_hash))]
    StepTimeout {
        step_index: usize,
        expected_target_hash: u64,
        last_observed_hash: Option<u64>,
        last_observed_package: String,
        timeout_ms: u64,
    },
    #[error("Human confirmation required before executing '{}' at step {}/{}: {}.", .0.action_signature, .0.step_index + 1, .0.total_steps, .0.hazard_reason)]
    RequiresConfirmation(HazardousConfirmationRequest),
    #[error("Secure/biometric screen ({reason} in {active_package}) remained locked past {timeout_seconds}s handoff timeout.")]
    SecureScreenTimeout {
        reason: String,
        active_package: String,
        timeout_seconds: u64,
    },
}

fn format_opt_hash(h: Option<u64>) -> String {
    h.map(|v| format!("0x{v:016x}"))
        .unwrap_or_else(|| "nil".to_string())
}

#[async_trait]
pub trait MacroActionExecutor: Send + Sync {
    async fn execute(&self, action: &AttributedUIAction, serial: &str) -> Result<(), PathDeviationError>;

    /// Returns `true` when this executor currently injects touch gestures via native zero-fling
    /// daemon `UiAutomation.injectInputEvent` (with ease-out + terminal hold), allowing wider
    /// scroll strokes, ~145ms drags, and tight post-swipe quiescence.
    async fn supports_zero_fling_swipes(&self, _serial: &str) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MacroExecutionConfiguration {
    pub step_timeout_ms: u64,
    pub post_settle_quiescence_ms: u64,
    pub settle_similarity_threshold: f64,
    pub allow_hazardous_actions: bool,
    pub enable_forward_skip_recovery: bool,
    pub enable_overlay_swipe_dismiss: bool,
    pub enable_smart_scroll_to_selector: bool,
    pub record_edge_failures_on_timeout: bool,
    pub secure_handoff_timeout_secs: f64,
}

impl Default for MacroExecutionConfiguration {
    fn default() -> Self {
        Self {
            step_timeout_ms: 2000,
            post_settle_quiescence_ms: 40,
            settle_similarity_threshold: 0.80,
            allow_hazardous_actions: false,
            enable_forward_skip_recovery: true,
            enable_overlay_swipe_dismiss: true,
            enable_smart_scroll_to_selector: true,
            record_edge_failures_on_timeout: true,
            secure_handoff_timeout_secs: 60.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroStepReport {
    pub step_index: usize,
    pub edge_id: Uuid,
    pub action_signature: String,
    pub target_hash: u64,
    pub elapsed_ms: u64,
    pub matched_by_exact_hash: bool,
    #[serde(default)]
    pub was_skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroExecutionReport {
    pub start_hash: u64,
    pub final_hash: u64,
    pub steps: Vec<MacroStepReport>,
    pub total_elapsed_ms: u64,
    #[serde(default)]
    pub recovered_steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MacroExecutionEvent {
    PathComputed {
        #[serde(rename = "totalSteps")]
        total_steps: usize,
        #[serde(rename = "targetHint")]
        target_hint: Option<String>,
    },
    ExecutionStarted {
        #[serde(rename = "totalSteps")]
        total_steps: usize,
    },
    WillExecuteAction {
        #[serde(rename = "stepIndex")]
        step_index: usize,
        #[serde(rename = "totalSteps")]
        total_steps: usize,
        action: AttributedUIAction,
    },
    StepSettled {
        #[serde(rename = "stepIndex")]
        step_index: usize,
        #[serde(rename = "totalSteps")]
        total_steps: usize,
        report: MacroStepReport,
    },
    Completed {
        report: MacroExecutionReport,
    },
    Failed {
        message: String,
    },
    AwaitingConfirmation {
        #[serde(rename = "stepIndex")]
        step_index: usize,
        #[serde(rename = "totalSteps")]
        total_steps: usize,
        action: AttributedUIAction,
        reason: String,
        request: HazardousConfirmationRequest,
    },
    AwaitingChatInput {
        prompt: String,
        #[serde(rename = "focusX")]
        focus_x: Option<i32>,
        #[serde(rename = "focusY")]
        focus_y: Option<i32>,
    },
    AwaitingBiometrics {
        reason: String,
        #[serde(rename = "activePackage")]
        active_package: String,
    },
    BiometricsCleared,
    RecoveryAction {
        kind: String,
        detail: String,
    },
}
