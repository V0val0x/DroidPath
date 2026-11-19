use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::broadcast;

use crate::protocol::{ShadowUINode, WireSecureScreenPayload};
use crate::shadow_dom::{BoundsRect, ShadowDOMUpdate, ShadowDomState, ShadowTreeIndex};
use crate::spatial_graph::{
    AttributedUIAction, HazardCategory, HazardousActionClassifier, ScreenLayoutHasher,
    ScreenSemanticFingerprint, ScreenStateVertex, ScreenTransitionEdge, SpatialGraphEngine,
    UIActionKind,
};

use super::executor::{
    HazardousConfirmationRequest, MacroActionExecutor, MacroExecutionConfiguration,
    MacroExecutionEvent, MacroExecutionReport, MacroStepReport, PathDeviationError,
};

pub type MacroEventCallback = Arc<dyn Fn(MacroExecutionEvent) + Send + Sync>;

struct SettleTarget {
    step_index: usize,
    target_hash: u64,
    expected_vertex: Option<ScreenStateVertex>,
}

enum SettleOutcome {
    Settled {
        matched_step_index: usize,
        hash: u64,
        matched_by_exact_hash: bool,
    },
    InterruptedBySecureScreen,
}

enum SmartScrollStepOutcome {
    Found(Box<(AttributedUIAction, ShadowUINode)>),
    Moved,
    BoundaryOrUnavailable,
}

/// Race-free asynchronous state machine that sequentially executes `[ScreenTransitionEdge]` paths,
/// subscribing to `ShadowDomState` updates before dispatching each UI action and enforcing settle deadlines.
pub struct MacroExecutionEngine {
    dom_state: Arc<ShadowDomState>,
    graph_engine: Arc<SpatialGraphEngine>,
    executor: Arc<dyn MacroActionExecutor>,
    hasher: ScreenLayoutHasher,
    configuration: MacroExecutionConfiguration,
    on_event: Option<MacroEventCallback>,
    is_executing: AtomicBool,
    event_tx: broadcast::Sender<MacroExecutionEvent>,
}

impl MacroExecutionEngine {
    pub fn new(
        dom_state: Arc<ShadowDomState>,
        graph_engine: Arc<SpatialGraphEngine>,
        executor: Arc<dyn MacroActionExecutor>,
        hasher: ScreenLayoutHasher,
        configuration: MacroExecutionConfiguration,
        on_event: Option<MacroEventCallback>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(128);
        Self {
            dom_state,
            graph_engine,
            executor,
            hasher,
            configuration,
            on_event,
            is_executing: AtomicBool::new(false),
            event_tx,
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<MacroExecutionEvent> {
        self.event_tx.subscribe()
    }

    fn emit(&self, event: MacroExecutionEvent) {
        let _ = self.event_tx.send(event.clone());
        if let Some(ref cb) = self.on_event {
            cb(event);
        }
    }

    pub async fn execute(
        &self,
        path: &[ScreenTransitionEdge],
        serial: &str,
    ) -> Result<MacroExecutionReport, PathDeviationError> {
        if self
            .is_executing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(PathDeviationError::ExecutionAlreadyInProgress);
        }
        struct ResetGuard<'a>(&'a AtomicBool);
        impl Drop for ResetGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = ResetGuard(&self.is_executing);

        let res = self.execute_inner(path, serial).await;
        match &res {
            Err(PathDeviationError::RequiresConfirmation(_)) => {}
            Err(err) => self.emit(MacroExecutionEvent::Failed {
                message: err.to_string(),
            }),
            Ok(_) => {}
        }
        res
    }

    async fn execute_inner(
        &self,
        path: &[ScreenTransitionEdge],
        serial: &str,
    ) -> Result<MacroExecutionReport, PathDeviationError> {
        let total_start = Instant::now();
        let _ = self.await_secure_handoff_if_active().await?;

        let initial_index = self
            .dom_state
            .current_index()
            .await
            .ok_or(PathDeviationError::ShadowStreamUnavailable)?;

        let initial_fp = self.hasher.fingerprint(&initial_index);
        let Some(first_edge) = path.first() else {
            return Ok(MacroExecutionReport {
                start_hash: initial_fp.layout_hash,
                final_hash: initial_fp.layout_hash,
                steps: Vec::new(),
                total_elapsed_ms: 0,
                recovered_steps: Vec::new(),
            });
        };

        let graph_vertices = self.graph_engine.vertices_map().await;
        let expected_vertices: Vec<Option<ScreenStateVertex>> = path
            .iter()
            .map(|e| graph_vertices.get(&e.to_hash).cloned())
            .collect();

        let matches_from = Self::matches_vertex(
            &initial_fp,
            first_edge.from_hash,
            graph_vertices.get(&first_edge.from_hash),
            self.configuration.settle_similarity_threshold,
        )
        .0;

        let matches_any_target = if self.configuration.enable_forward_skip_recovery {
            (0..path.len()).any(|idx| {
                Self::matches_vertex(
                    &initial_fp,
                    path[idx].to_hash,
                    expected_vertices[idx].as_ref(),
                    self.configuration.settle_similarity_threshold,
                )
                .0
            })
        } else {
            Self::matches_vertex(
                &initial_fp,
                first_edge.to_hash,
                expected_vertices[0].as_ref(),
                self.configuration.settle_similarity_threshold,
            )
            .0
        };

        if !matches_from && !matches_any_target {
            return Err(PathDeviationError::UnexpectedStartState {
                expected_hash: first_edge.from_hash,
                actual_hash: Some(initial_fp.layout_hash),
                actual_package: initial_fp.active_package,
            });
        }

        let mut reports = Vec::with_capacity(path.len());
        let mut recovered_steps = Vec::new();
        let mut last_settled_hash = initial_fp.layout_hash;
        let final_target_hash = path.last().map(|e| e.to_hash).unwrap_or(first_edge.to_hash);

        self.emit(MacroExecutionEvent::ExecutionStarted {
            total_steps: path.len(),
        });

        let mut step_index = 0usize;
        while step_index < path.len() {
            if self.await_secure_handoff_if_active().await? {
                recovered_steps.push("secure_handoff".to_string());
            }

            let edge = &path[step_index];

            // Pre-step settlement / forward-skip scan (#1)
            if let Some(live_index) = self.dom_state.current_index().await {
                let live_fp = self.hasher.fingerprint(&live_index);
                let scan_ceiling = if self.configuration.enable_forward_skip_recovery {
                    path.len() - 1
                } else {
                    step_index
                };
                if let Some(matched_jump_idx) = Self::furthest_matched_step_index(
                    &live_fp,
                    path,
                    &expected_vertices,
                    step_index,
                    scan_ceiling,
                    self.configuration.settle_similarity_threshold,
                ) {
                    last_settled_hash = live_fp.layout_hash;
                    if matched_jump_idx > step_index {
                        let detail = format!(
                            "Skipped steps {}...{} (already at screen {}/{})",
                            step_index + 1,
                            matched_jump_idx + 1,
                            matched_jump_idx + 1,
                            path.len()
                        );
                        recovered_steps
                            .push(format!("forward_skip:{step_index}->{matched_jump_idx}"));
                        self.emit(MacroExecutionEvent::RecoveryAction {
                            kind: "forward_skip".to_string(),
                            detail,
                        });
                    }
                    for skipped_idx in step_index..=matched_jump_idx {
                        let skipped_edge = &path[skipped_idx];
                        let match_exact = live_fp.layout_hash == skipped_edge.to_hash;
                        let step_report = MacroStepReport {
                            step_index: skipped_idx,
                            edge_id: skipped_edge.id,
                            action_signature: skipped_edge.action_signature.clone(),
                            target_hash: skipped_edge.to_hash,
                            elapsed_ms: 0,
                            matched_by_exact_hash: match_exact,
                            was_skipped: true,
                        };
                        reports.push(step_report.clone());
                        if skipped_idx == matched_jump_idx {
                            self.emit(MacroExecutionEvent::StepSettled {
                                step_index: skipped_idx,
                                total_steps: path.len(),
                                report: step_report,
                            });
                        }
                    }
                    step_index = matched_jump_idx + 1;
                    continue;
                }
            }

            let Some(ref raw_action) = edge.action else {
                return Err(PathDeviationError::MissingActionPayload {
                    step_index,
                    edge_id: edge.id,
                });
            };

            let (mut resolved_action, mut matched_node) =
                self.resolve_live_action_with_node(raw_action).await;

            // Smart Scroll-to-Selector: if tap selector is off-screen inside a scrollable container,
            // perform up to 2 search scrolls (down / reverse up at bottom) to reveal & lock onto the node.
            if self.configuration.enable_smart_scroll_to_selector
                && matched_node.is_none()
                && raw_action.kind == UIActionKind::Tap
            {
                if let Some((scrolled_action, scrolled_node)) = self
                    .smart_scroll_to_selector(raw_action, serial, &mut recovered_steps)
                    .await?
                {
                    resolved_action = scrolled_action;
                    matched_node = Some(scrolled_node);
                }
            }

            // Hazardous / irreversible transaction HITL check (#2, Variant A)
            let live_label = matched_node.as_ref().map(|n| {
                if !n.text.is_empty() {
                    n.text.as_str()
                } else {
                    n.content_desc.as_str()
                }
            });
            let live_assessment = HazardousActionClassifier::assess(
                Some(&resolved_action),
                &resolved_action.signature(),
                live_label,
                matched_node.as_ref().map(|n| n.resource_id.as_str()),
            );
            let edge_assessment = HazardousActionClassifier::assess(
                edge.action.as_ref(),
                &edge.action_signature,
                None,
                None,
            );
            let is_hazardous =
                edge.is_hazardous || edge_assessment.is_hazardous || live_assessment.is_hazardous;
            if is_hazardous && !self.configuration.allow_hazardous_actions {
                let category = live_assessment
                    .category
                    .or(edge_assessment.category)
                    .unwrap_or(HazardCategory::Destructive);
                let live_reason = live_assessment.reason();
                let edge_reason = edge_assessment.reason();
                let reason = if !live_reason.is_empty() {
                    live_reason
                } else if !edge_reason.is_empty() {
                    edge_reason
                } else {
                    edge.hazard_reason
                        .clone()
                        .unwrap_or_else(|| "Irreversible action requires human approval".to_string())
                };
                let current_live_hash = match self.dom_state.current_index().await {
                    Some(idx) => self.hasher.fingerprint(&idx).layout_hash,
                    None => last_settled_hash,
                };
                let req = HazardousConfirmationRequest {
                    step_index,
                    total_steps: path.len(),
                    action_signature: resolved_action.signature(),
                    target_description: resolved_action.target_description.clone(),
                    hazard_category: category,
                    hazard_reason: reason.clone(),
                    current_hash: current_live_hash,
                    target_hash: final_target_hash,
                    steps_completed: reports.len(),
                    tap_x: resolved_action.x,
                    tap_y: resolved_action.y,
                };
                self.emit(MacroExecutionEvent::AwaitingConfirmation {
                    step_index,
                    total_steps: path.len(),
                    action: resolved_action,
                    reason,
                    request: req.clone(),
                });
                return Err(PathDeviationError::RequiresConfirmation(req));
            }

            // Reflexive push-notification banner swipe-up (#3)
            if self.configuration.enable_overlay_swipe_dismiss
                && resolved_action.kind == UIActionKind::Tap
            {
                let (updated_act, updated_node) = self
                    .dismiss_occluding_system_banner_if_present(
                        resolved_action,
                        raw_action,
                        serial,
                        &mut recovered_steps,
                    )
                    .await?;
                resolved_action = updated_act;
                if updated_node.is_some() {
                    matched_node = updated_node;
                }
                let _ = matched_node;
            }

            // Suffix targets furthest-first for in-flight forward-skip settle (#1)
            let target_max_idx = if self.configuration.enable_forward_skip_recovery {
                path.len() - 1
            } else {
                step_index
            };
            let suffix_targets: Vec<SettleTarget> = (step_index..=target_max_idx)
                .rev()
                .map(|idx| SettleTarget {
                    step_index: idx,
                    target_hash: path[idx].to_hash,
                    expected_vertex: expected_vertices[idx].clone(),
                })
                .collect();

            // CRITICAL RACE-FREE ORDER:
            // Subscribe to updates & secure screen BEFORE dispatching input.
            let update_rx = self.dom_state.subscribe_updates();
            let secure_rx = self.dom_state.subscribe_secure_screen();
            self.graph_engine
                .record_pending_action(resolved_action.clone())
                .await;

            self.emit(MacroExecutionEvent::WillExecuteAction {
                step_index,
                total_steps: path.len(),
                action: resolved_action.clone(),
            });

            let step_start = Instant::now();
            let settle_res = self
                .execute_and_wait_for_settle(
                    step_index,
                    &resolved_action,
                    serial,
                    &suffix_targets,
                    update_rx,
                    secure_rx,
                )
                .await;
            let settle_outcome = match settle_res {
                Ok(outcome) => outcome,
                Err(err @ PathDeviationError::StepTimeout { .. }) => {
                    if self.configuration.record_edge_failures_on_timeout {
                        let _ = self.graph_engine.record_edge_failure(edge.id).await;
                    }
                    return Err(err);
                }
                Err(err) => return Err(err),
            };

            let (matched_idx, settled_hash, settled_exact) = match settle_outcome {
                SettleOutcome::Settled {
                    matched_step_index,
                    hash,
                    matched_by_exact_hash,
                } => (matched_step_index, hash, matched_by_exact_hash),

                SettleOutcome::InterruptedBySecureScreen => {
                    if self.await_secure_handoff_if_active().await? {
                        recovered_steps.push("secure_handoff".to_string());
                    }
                    match self
                        .settle_after_secure_handoff(step_index, &suffix_targets, last_settled_hash)
                        .await
                    {
                        Ok(settled) => settled,
                        Err(err @ PathDeviationError::StepTimeout { .. }) => {
                            if self.configuration.record_edge_failures_on_timeout {
                                let _ = self.graph_engine.record_edge_failure(edge.id).await;
                            }
                            return Err(err);
                        }
                        Err(err) => return Err(err),
                    }
                }
            };

            self.graph_engine.record_edge_success(edge.id).await;

            let elapsed_ms = step_start.elapsed().as_millis() as u64;
            last_settled_hash = settled_hash;

            let primary_report = MacroStepReport {
                step_index,
                edge_id: edge.id,
                action_signature: resolved_action.signature(),
                target_hash: edge.to_hash,
                elapsed_ms,
                matched_by_exact_hash: if matched_idx == step_index {
                    settled_exact
                } else {
                    false
                },
                was_skipped: false,
            };
            reports.push(primary_report.clone());
            self.emit(MacroExecutionEvent::StepSettled {
                step_index,
                total_steps: path.len(),
                report: primary_report,
            });

            if matched_idx > step_index {
                let detail = format!(
                    "Forward-skipped {} intermediate step(s) after landing on step {}/{}",
                    matched_idx - step_index,
                    matched_idx + 1,
                    path.len()
                );
                recovered_steps.push(format!("forward_skip:{step_index}->{matched_idx}"));
                self.emit(MacroExecutionEvent::RecoveryAction {
                    kind: "forward_skip".to_string(),
                    detail,
                });

                for skipped_idx in (step_index + 1)..=matched_idx {
                    let skipped_edge = &path[skipped_idx];
                    let skip_report = MacroStepReport {
                        step_index: skipped_idx,
                        edge_id: skipped_edge.id,
                        action_signature: skipped_edge.action_signature.clone(),
                        target_hash: skipped_edge.to_hash,
                        elapsed_ms: 0,
                        matched_by_exact_hash: if skipped_idx == matched_idx {
                            settled_exact
                        } else {
                            false
                        },
                        was_skipped: true,
                    };
                    reports.push(skip_report.clone());
                    if skipped_idx == matched_idx {
                        self.emit(MacroExecutionEvent::StepSettled {
                            step_index: skipped_idx,
                            total_steps: path.len(),
                            report: skip_report,
                        });
                    }
                }
            }

            step_index = matched_idx + 1;

            if step_index < path.len() && self.configuration.post_settle_quiescence_ms > 0 {
                // Wait for 120Hz RecyclerView fling deceleration before a subsequent tap so Android
                // reaches SCROLL_STATE_IDLE instead of swallowing the tap as a fling-brake touch.
                let next_is_swipe = path[step_index]
                    .action
                    .as_ref()
                    .is_some_and(|a| a.kind == UIActionKind::Swipe);
                let quiescence_ms = if resolved_action.kind == UIActionKind::Swipe {
                    let min_wait = if next_is_swipe { 280 } else { 480 };
                    self.configuration.post_settle_quiescence_ms.max(min_wait)
                } else {
                    self.configuration.post_settle_quiescence_ms
                };
                tokio::time::sleep(Duration::from_millis(quiescence_ms)).await;
            }
        }

        let report = MacroExecutionReport {
            start_hash: initial_fp.layout_hash,
            final_hash: last_settled_hash,
            steps: reports,
            total_elapsed_ms: total_start.elapsed().as_millis() as u64,
            recovered_steps,
        };
        self.emit(MacroExecutionEvent::Completed {
            report: report.clone(),
        });
        Ok(report)
    }

    async fn await_secure_handoff_if_active(&self) -> Result<bool, PathDeviationError> {
        let (active, reason_opt, pkg) = self.dom_state.secure_screen_info().await;
        if !active {
            return Ok(false);
        }
        let reason = reason_opt.unwrap_or_else(|| "biometric_or_pin".to_string());
        self.emit(MacroExecutionEvent::AwaitingBiometrics {
            reason: reason.clone(),
            active_package: pkg.clone(),
        });
        let timeout = self.configuration.secure_handoff_timeout_secs.max(0.5);
        let cleared = self
            .dom_state
            .wait_until_secure_screen_cleared(timeout)
            .await;
        if !cleared {
            return Err(PathDeviationError::SecureScreenTimeout {
                reason,
                active_package: pkg,
                timeout_seconds: timeout.round() as u64,
            });
        }
        self.emit(MacroExecutionEvent::BiometricsCleared);
        Ok(true)
    }

    async fn settle_after_secure_handoff(
        &self,
        step_index: usize,
        suffix_targets: &[SettleTarget],
        last_known_hash: u64,
    ) -> Result<(usize, u64, bool), PathDeviationError> {
        if let Some(live_index) = self.dom_state.current_index().await {
            if !live_index.ordered_nodes.is_empty() {
                let live_fp = self.hasher.fingerprint(&live_index);
                for target in suffix_targets {
                    let (matched, exact) = Self::matches_vertex(
                        &live_fp,
                        target.target_hash,
                        target.expected_vertex.as_ref(),
                        self.configuration.settle_similarity_threshold,
                    );
                    if matched {
                        return Ok((target.step_index, live_fp.layout_hash, exact));
                    }
                }
            }
        }

        let mut update_rx = self.dom_state.subscribe_updates();
        let timeout = Duration::from_millis(self.configuration.step_timeout_ms.max(50));
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                res = update_rx.recv() => {
                    match res {
                        Ok(update) => {
                            if update.is_secure_screen_active {
                                continue;
                            }
                            let fp = self.hasher.fingerprint(&update.index);
                            for target in suffix_targets {
                                let (matched, exact) = Self::matches_vertex(
                                    &fp,
                                    target.target_hash,
                                    target.expected_vertex.as_ref(),
                                    self.configuration.settle_similarity_threshold,
                                );
                                if matched {
                                    return Ok((target.step_index, fp.layout_hash, exact));
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        let live = self.dom_state.current_index().await;
        let live_hash = live
            .as_ref()
            .map(|idx| self.hasher.fingerprint(idx).layout_hash)
            .unwrap_or(last_known_hash);
        let live_pkg = live
            .as_ref()
            .map(|idx| idx.active_package.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let primary_target = suffix_targets.last().map(|t| t.target_hash).unwrap_or(0);
        Err(PathDeviationError::StepTimeout {
            step_index,
            expected_target_hash: primary_target,
            last_observed_hash: Some(live_hash),
            last_observed_package: live_pkg,
            timeout_ms: self.configuration.step_timeout_ms,
        })
    }

    async fn execute_and_wait_for_settle(
        &self,
        step_index: usize,
        action: &AttributedUIAction,
        serial: &str,
        targets: &[SettleTarget],
        mut update_rx: broadcast::Receiver<ShadowDOMUpdate>,
        mut secure_rx: broadcast::Receiver<WireSecureScreenPayload>,
    ) -> Result<SettleOutcome, PathDeviationError> {
        let primary_target_hash = targets.last().map(|t| t.target_hash).unwrap_or(0);
        let timeout_ms = self.configuration.step_timeout_ms.max(50);
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);

        let exec_fut = self.executor.execute(action, serial);
        tokio::pin!(exec_fut);

        let mut action_dispatched = false;
        let mut settled_outcome: Option<(usize, u64, bool)> = None;

        loop {
            tokio::select! {
                exec_res = &mut exec_fut, if !action_dispatched => {
                    exec_res.map_err(|err| match err {
                        PathDeviationError::ActionDispatchFailed { action_signature, underlying_message, .. } => {
                            PathDeviationError::ActionDispatchFailed {
                                step_index: step_index as i32,
                                action_signature,
                                underlying_message,
                            }
                        }
                        other => other,
                    })?;
                    action_dispatched = true;
                    if let Some((idx, hash, exact)) = settled_outcome {
                        return Ok(SettleOutcome::Settled {
                            matched_step_index: idx,
                            hash,
                            matched_by_exact_hash: exact,
                        });
                    }
                }

                upd = update_rx.recv() => {
                    match upd {
                        Ok(update) => {
                            if update.is_secure_screen_active {
                                continue;
                            }
                            let fp = self.hasher.fingerprint(&update.index);
                            for target in targets {
                                let (matched, exact) = Self::matches_vertex(
                                    &fp,
                                    target.target_hash,
                                    target.expected_vertex.as_ref(),
                                    self.configuration.settle_similarity_threshold,
                                );
                                if matched {
                                    settled_outcome = Some((target.step_index, fp.layout_hash, exact));
                                    if action_dispatched {
                                        return Ok(SettleOutcome::Settled {
                                            matched_step_index: target.step_index,
                                            hash: fp.layout_hash,
                                            matched_by_exact_hash: exact,
                                        });
                                    }
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }

                sec = secure_rx.recv() => {
                    match sec {
                        Ok(payload) => {
                            if payload.active {
                                if let Some((idx, hash, exact)) = settled_outcome {
                                    return Ok(SettleOutcome::Settled {
                                        matched_step_index: idx,
                                        hash,
                                        matched_by_exact_hash: exact,
                                    });
                                }
                                return Ok(SettleOutcome::InterruptedBySecureScreen);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {}
                    }
                }

                _ = &mut deadline => {
                    if let Some((idx, hash, exact)) = settled_outcome {
                        return Ok(SettleOutcome::Settled {
                            matched_step_index: idx,
                            hash,
                            matched_by_exact_hash: exact,
                        });
                    }
                    if self.dom_state.is_secure_screen_active().await {
                        return Ok(SettleOutcome::InterruptedBySecureScreen);
                    }
                    let live = self.dom_state.current_index().await;
                    let live_hash = live.as_ref().map(|idx| self.hasher.fingerprint(idx).layout_hash);
                    let live_pkg = live
                        .as_ref()
                        .map(|idx| idx.active_package.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(PathDeviationError::StepTimeout {
                        step_index,
                        expected_target_hash: primary_target_hash,
                        last_observed_hash: live_hash,
                        last_observed_package: live_pkg,
                        timeout_ms,
                    });
                }
            }
        }

        Err(PathDeviationError::ShadowStreamUnavailable)
    }

    async fn dismiss_occluding_system_banner_if_present(
        &self,
        mut current_action: AttributedUIAction,
        raw_action: &AttributedUIAction,
        serial: &str,
        recovered_steps: &mut Vec<String>,
    ) -> Result<(AttributedUIAction, Option<ShadowUINode>), PathDeviationError> {
        let mut current_node = None;

        for attempt in 0..2 {
            let (Some(tap_x), Some(tap_y)) = (current_action.x, current_action.y) else {
                break;
            };
            let Some(live_index) = self.dom_state.current_index().await else {
                break;
            };
            let Some(banner_rect) = Self::occluding_system_banner_rect(&live_index, tap_x, tap_y)
            else {
                break;
            };

            let swipe_x = ((banner_rect.min_x + banner_rect.max_x) / 2).max(1);
            let start_y = (banner_rect.max_y - 20).max(20);
            let end_y = (banner_rect.min_y - 120).max(10);
            let swipe_action = AttributedUIAction::swipe(
                "Dismiss occluding notification",
                swipe_x,
                start_y,
                swipe_x,
                end_y,
                Some(160),
            );

            recovered_steps.push(format!("dismiss_notification_overlay_{}", attempt + 1));
            self.emit(MacroExecutionEvent::RecoveryAction {
                kind: "dismiss_notification_overlay".to_string(),
                detail: "Swiped up occluding system notification".to_string(),
            });

            // Subscribe BEFORE swipe and explicitly do NOT call graph_engine.record_pending_action
            let mut update_rx = self.dom_state.subscribe_updates();
            self.executor.execute(&swipe_action, serial).await?;

            let deadline = tokio::time::sleep(Duration::from_millis(450));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    res = update_rx.recv() => {
                        match res {
                            Ok(upd) => {
                                if Self::occluding_system_banner_rect(&upd.index, tap_x, tap_y).is_none() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }

            let (refreshed_act, refreshed_node) =
                self.resolve_live_action_with_node(raw_action).await;
            current_action = refreshed_act;
            current_node = refreshed_node;
        }

        Ok((current_action, current_node))
    }

    pub fn occluding_system_banner_rect(
        index: &ShadowTreeIndex,
        tap_x: i32,
        tap_y: i32,
    ) -> Option<BoundsRect> {
        let has_app_nodes = index.ordered_nodes.iter().any(|n| {
            n.visible && !n.package_name.is_empty() && n.package_name != "com.android.systemui"
        });
        if !has_app_nodes {
            return None;
        }

        let mut union_rect: Option<BoundsRect> = None;
        for node in &index.ordered_nodes {
            if !node.visible {
                continue;
            }
            let is_system_ui = node.package_name == "com.android.systemui"
                || node.resource_id.starts_with("com.android.systemui:");
            if !is_system_ui {
                continue;
            }
            let Some(rect) = ShadowTreeIndex::parse_bounds_rect(&node.bounds) else {
                continue;
            };
            if !rect.contains(tap_x, tap_y) {
                continue;
            }

            let width = rect.max_x - rect.min_x;
            let height = rect.max_y - rect.min_y;
            let is_status_bar = rect.min_y <= 12 && rect.max_y <= 120;
            let is_full_screen_overlay = height > 750;
            if is_status_bar || is_full_screen_overlay {
                continue;
            }

            let rid_lower = node.resource_id.to_lowercase();
            let has_banner_hint = rid_lower.contains("notification")
                || rid_lower.contains("heads_up")
                || rid_lower.contains("alert");
            if !((height >= 70 && width >= 250 && rect.min_y < 650) || has_banner_hint) {
                continue;
            }

            union_rect = Some(match union_rect {
                Some(existing) => BoundsRect {
                    min_x: existing.min_x.min(rect.min_x),
                    min_y: existing.min_y.min(rect.min_y),
                    max_x: existing.max_x.max(rect.max_x),
                    max_y: existing.max_y.max(rect.max_y),
                },
                None => rect,
            });
        }
        union_rect
    }

    /// Finds the largest visible non-system `scrollable` container bounds on screen.
    pub fn largest_scrollable_rect(
        index: &ShadowTreeIndex,
        ignored_packages: &HashSet<String>,
    ) -> Option<BoundsRect> {
        let skip_ignored = !ignored_packages.contains(&index.active_package);
        let mut best: Option<BoundsRect> = None;

        for node in &index.ordered_nodes {
            if !node.visible || !node.scrollable {
                continue;
            }
            if skip_ignored
                && (!node.package_name.is_empty() && ignored_packages.contains(&node.package_name)
                    || ignored_packages
                        .iter()
                        .any(|pkg| node.resource_id.starts_with(&format!("{pkg}:"))))
            {
                continue;
            }
            // Skip horizontal-only pagers and launcher home-screen workspaces where a vertical
            // downward swipe pulls down the Android Notification Shade instead of scrolling a list.
            let cls_lower = node.class_name.to_lowercase();
            if cls_lower.contains("horizontalscrollview") || cls_lower.contains("viewpager") {
                continue;
            }
            let rid_lower = node.resource_id.to_lowercase();
            if rid_lower.ends_with(":id/workspace")
                || rid_lower.ends_with("/workspace")
                || rid_lower.ends_with(":id/hotseat")
                || rid_lower.ends_with("/hotseat")
                || rid_lower.ends_with(":id/smartspace_card_pager")
                || rid_lower.ends_with("/smartspace_card_pager")
            {
                continue;
            }
            let Some(rect) = ShadowTreeIndex::parse_bounds_rect(&node.bounds) else {
                continue;
            };
            let width = rect.max_x - rect.min_x;
            let height = rect.max_y - rect.min_y;
            if width < 120 || height < 120 {
                continue;
            }
            if best.map(|b| rect.area() > b.area()).unwrap_or(true) {
                best = Some(rect);
            }
        }
        best
    }

    /// Extracts an ordered signature of visible labeled items inside `container` to detect scroll movement vs list boundaries.
    pub fn scroll_viewport_signature(
        index: &ShadowTreeIndex,
        container: BoundsRect,
        ignored_packages: &HashSet<String>,
    ) -> Vec<(String, i32)> {
        let skip_ignored = !ignored_packages.contains(&index.active_package);
        let mut items = Vec::new();
        for node in &index.ordered_nodes {
            if !node.visible {
                continue;
            }
            if skip_ignored
                && (!node.package_name.is_empty() && ignored_packages.contains(&node.package_name)
                    || ignored_packages
                        .iter()
                        .any(|pkg| node.resource_id.starts_with(&format!("{pkg}:"))))
            {
                continue;
            }
            if !container.contains(node.center_x, node.center_y) {
                continue;
            }
            let label = if !node.text.is_empty() {
                &node.text
            } else if !node.content_desc.is_empty() {
                &node.content_desc
            } else {
                continue;
            };
            items.push((label.clone(), node.center_y));
        }
        items
    }

    /// Performs controlled search scrolls (up to 2 downward; if not found, reverses upward to cover
    /// items positioned above a mid-scroll or bottom-positioned viewport) when a tap selector is absent
    /// from the currently visible viewport of a scrollable list.
    async fn smart_scroll_to_selector(
        &self,
        raw_action: &AttributedUIAction,
        serial: &str,
        recovered_steps: &mut Vec<String>,
    ) -> Result<Option<(AttributedUIAction, ShadowUINode)>, PathDeviationError> {
        let has_selector = raw_action
            .selector_text
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            || raw_action
                .selector_resource_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
            || raw_action
                .selector_content_desc
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty());
        if !has_selector {
            return Ok(None);
        }

        // Phase 1: Scroll downward (up to 2 attempts, stopping early on bottom boundary).
        let mut down_moves = 0usize;
        for attempt in 1..=2usize {
            match self
                .perform_smart_scroll_step(raw_action, serial, true, attempt, 2)
                .await?
            {
                SmartScrollStepOutcome::Found(found) => {
                    recovered_steps.push(format!("smart_scroll_to_selector:down:{attempt}"));
                    return Ok(Some(*found));
                }
                SmartScrollStepOutcome::Moved => {
                    down_moves += 1;
                }
                SmartScrollStepOutcome::BoundaryOrUnavailable => {
                    break;
                }
            }
        }

        // Phase 2: Reverse upward to recover targets above a mid-scroll or bottom viewport.
        let up_budget = (down_moves + 1).clamp(2, 3);
        for attempt in 1..=up_budget {
            match self
                .perform_smart_scroll_step(raw_action, serial, false, attempt, up_budget)
                .await?
            {
                SmartScrollStepOutcome::Found(found) => {
                    recovered_steps.push(format!("smart_scroll_to_selector:up:{attempt}"));
                    return Ok(Some(*found));
                }
                SmartScrollStepOutcome::Moved => {}
                SmartScrollStepOutcome::BoundaryOrUnavailable => {
                    break;
                }
            }
        }

        Ok(None)
    }

    async fn perform_smart_scroll_step(
        &self,
        raw_action: &AttributedUIAction,
        serial: &str,
        scrolling_down: bool,
        attempt: usize,
        budget: usize,
    ) -> Result<SmartScrollStepOutcome, PathDeviationError> {
        let Some(live_index) = self.dom_state.current_index().await else {
            return Ok(SmartScrollStepOutcome::BoundaryOrUnavailable);
        };
        let Some(scroll_rect) =
            Self::largest_scrollable_rect(&live_index, &self.hasher.ignored_packages)
        else {
            return Ok(SmartScrollStepOutcome::BoundaryOrUnavailable);
        };

        let before_hash = self.hasher.fingerprint(&live_index).layout_hash;
        let before_sig = Self::scroll_viewport_signature(
            &live_index,
            scroll_rect,
            &self.hasher.ignored_packages,
        );

        let cx = ((scroll_rect.min_x + scroll_rect.max_x) / 2).max(1);
        let span_y = scroll_rect.max_y - scroll_rect.min_y;
        let y_high = scroll_rect.min_y + (span_y * 72) / 100;
        let y_low = scroll_rect.min_y + (span_y * 28) / 100;
        let (start_y, end_y, dir_label) = if scrolling_down {
            (y_high, y_low, "down")
        } else {
            (y_low, y_high, "up")
        };

        let swipe_action = AttributedUIAction::swipe(
            format!("Smart scroll {dir_label} for '{}'", raw_action.target_description),
            cx,
            start_y,
            cx,
            end_y,
            Some(500),
        );

        self.emit(MacroExecutionEvent::RecoveryAction {
            kind: "smart_scroll_to_selector".to_string(),
            detail: format!(
                "Scrolling {dir_label} ({attempt}/{budget}) to reveal '{}'",
                raw_action.target_description
            ),
        });

        // Subscribe BEFORE swipe and explicitly do NOT call graph_engine.record_pending_action
        let mut update_rx = self.dom_state.subscribe_updates();
        self.executor.execute(&swipe_action, serial).await?;

        let wait_ms = self.configuration.step_timeout_ms.clamp(150, 800);
        let deadline = tokio::time::sleep(Duration::from_millis(wait_ms));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                res = update_rx.recv() => {
                    match res {
                        Ok(upd) => {
                            if upd.is_secure_screen_active {
                                continue;
                            }
                            let next_hash = self.hasher.fingerprint(&upd.index).layout_hash;
                            let next_sig = Self::scroll_viewport_signature(
                                &upd.index,
                                scroll_rect,
                                &self.hasher.ignored_packages,
                            );
                            if next_hash != before_hash || next_sig != before_sig {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        if self.configuration.post_settle_quiescence_ms > 0 {
            let q_ms = self.configuration.post_settle_quiescence_ms.max(400);
            tokio::time::sleep(Duration::from_millis(q_ms)).await;
        }

        let (updated_act, matched) = self.resolve_live_action_with_node(raw_action).await;
        if let Some(node) = matched {
            return Ok(SmartScrollStepOutcome::Found(Box::new((updated_act, node))));
        }

        if let Some(after_index) = self.dom_state.current_index().await {
            let after_hash = self.hasher.fingerprint(&after_index).layout_hash;
            let after_sig = Self::scroll_viewport_signature(
                &after_index,
                scroll_rect,
                &self.hasher.ignored_packages,
            );
            if after_hash == before_hash && after_sig == before_sig {
                return Ok(SmartScrollStepOutcome::BoundaryOrUnavailable);
            }
            return Ok(SmartScrollStepOutcome::Moved);
        }

        Ok(SmartScrollStepOutcome::BoundaryOrUnavailable)
    }

    async fn resolve_live_action_with_node(
        &self,
        action: &AttributedUIAction,
    ) -> (AttributedUIAction, Option<ShadowUINode>) {
        if action.kind != UIActionKind::Tap {
            return (action.clone(), None);
        }

        let rid = action
            .selector_resource_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let text = action
            .selector_text
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let desc = action
            .selector_content_desc
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let current_index = self.dom_state.current_index().await;

        if rid.is_none() && text.is_none() && desc.is_none() {
            if let (Some(x), Some(y), Some(idx)) = (action.x, action.y, current_index.as_ref()) {
                if let Some(live_node) = idx.resolve_node_at(x, y) {
                    return (action.clone(), Some(live_node.clone()));
                }
            }
            return (action.clone(), None);
        }

        let visible_nodes: Vec<&ShadowUINode> = current_index
            .as_ref()
            .map(|idx| {
                idx.ordered_nodes
                    .iter()
                    .filter(|n| n.visible && (n.center_x > 0 || n.center_y > 0))
                    .collect()
            })
            .unwrap_or_default();

        let mut matched_node: Option<&ShadowUINode> = None;

        if !visible_nodes.is_empty() {
            let text_lower = text.map(|s| s.to_lowercase());
            let desc_lower = desc.map(|s| s.to_lowercase());

            // 1. Joint match across all recorded selectors
            let joint: Vec<&ShadowUINode> = visible_nodes
                .iter()
                .copied()
                .filter(|node| {
                    if let Some(r) = rid {
                        if !Self::node_matches_resource_id(node, r) {
                            return false;
                        }
                    }
                    if let Some(ref tl) = text_lower {
                        if !node.text.to_lowercase().contains(tl) {
                            return false;
                        }
                    }
                    if let Some(ref dl) = desc_lower {
                        if !node.content_desc.to_lowercase().contains(dl) {
                            return false;
                        }
                    }
                    true
                })
                .collect();
            if !joint.is_empty() {
                matched_node = Self::best_live_candidate(&joint, text_lower.as_deref(), desc_lower.as_deref());
            }

            // 2. Text / contentDesc fallback
            if matched_node.is_none() {
                if let Some(ref tl) = text_lower {
                    let text_cands: Vec<&ShadowUINode> = visible_nodes
                        .iter()
                        .copied()
                        .filter(|n| n.text.to_lowercase().contains(tl))
                        .collect();
                    matched_node = Self::best_live_candidate(
                        &text_cands,
                        text_lower.as_deref(),
                        desc_lower.as_deref(),
                    );
                }
            }
            if matched_node.is_none() {
                if let Some(ref dl) = desc_lower {
                    let desc_cands: Vec<&ShadowUINode> = visible_nodes
                        .iter()
                        .copied()
                        .filter(|n| n.content_desc.to_lowercase().contains(dl))
                        .collect();
                    matched_node = Self::best_live_candidate(
                        &desc_cands,
                        text_lower.as_deref(),
                        desc_lower.as_deref(),
                    );
                }
            }

            // 3. Unique resourceId fallback
            if matched_node.is_none() {
                if let Some(r) = rid {
                    let rid_cands: Vec<&ShadowUINode> = visible_nodes
                        .iter()
                        .copied()
                        .filter(|n| Self::node_matches_resource_id(n, r))
                        .collect();
                    if rid_cands.len() == 1 {
                        matched_node = Some(rid_cands[0]);
                    }
                }
            }
        }

        let Some(node) = matched_node else {
            return (action.clone(), None);
        };
        if !node.visible || (node.center_x <= 0 && node.center_y <= 0) {
            return (action.clone(), None);
        }

        let mut updated = action.clone();
        updated.x = Some(node.center_x);
        updated.y = Some(node.center_y);
        updated.timestamp = Utc::now();
        (updated, Some(node.clone()))
    }

    fn furthest_matched_step_index(
        fingerprint: &ScreenSemanticFingerprint,
        path: &[ScreenTransitionEdge],
        expected_vertices: &[Option<ScreenStateVertex>],
        scan_floor: usize,
        scan_ceiling: usize,
        threshold: f64,
    ) -> Option<usize> {
        if scan_floor > scan_ceiling || scan_ceiling >= path.len() {
            return None;
        }
        (scan_floor..=scan_ceiling).rev().find(|&k| {
            Self::matches_vertex(
                fingerprint,
                path[k].to_hash,
                expected_vertices[k].as_ref(),
                threshold,
            )
            .0
        })
    }

    fn node_matches_resource_id(node: &ShadowUINode, rid: &str) -> bool {
        if node.resource_id.is_empty() {
            return false;
        }
        if node.resource_id == rid {
            return true;
        }
        let node_lower = node.resource_id.to_lowercase();
        let rid_lower = rid.to_lowercase();
        node_lower == rid_lower
            || node_lower.ends_with(&format!("/{rid_lower}"))
            || node_lower.ends_with(&format!(":{rid_lower}"))
    }

    fn best_live_candidate<'a>(
        candidates: &[&'a ShadowUINode],
        lower_text: Option<&str>,
        lower_desc: Option<&str>,
    ) -> Option<&'a ShadowUINode> {
        candidates
            .iter()
            .copied()
            .max_by_key(|n| Self::candidate_score(n, lower_text, lower_desc))
    }

    fn candidate_score(
        node: &ShadowUINode,
        lower_text: Option<&str>,
        lower_desc: Option<&str>,
    ) -> i32 {
        let mut score = 0;
        if node.visible {
            score += 16;
        }
        if node.enabled {
            score += 8;
        }
        if node.clickable {
            score += 4;
        }
        if let Some(lt) = lower_text {
            if !lt.is_empty() {
                let nt = node.text.to_lowercase();
                if nt == lt {
                    score += 100;
                } else if nt.starts_with(lt) {
                    score += 50;
                } else if nt.contains(lt) {
                    score += 20;
                }
            }
        }
        if let Some(ld) = lower_desc {
            if !ld.is_empty() {
                let nd = node.content_desc.to_lowercase();
                if nd == ld {
                    score += 100;
                } else if nd.starts_with(ld) {
                    score += 50;
                } else if nd.contains(ld) {
                    score += 20;
                }
            }
        }
        score
    }

    fn matches_vertex(
        fingerprint: &ScreenSemanticFingerprint,
        expected_hash: u64,
        expected_vertex: Option<&ScreenStateVertex>,
        threshold: f64,
    ) -> (bool, bool) {
        if fingerprint.layout_hash == expected_hash {
            return (true, true);
        }
        let Some(expected) = expected_vertex else {
            return (false, false);
        };
        if expected.semantic_tokens.is_empty()
            || fingerprint.active_package != expected.active_package
        {
            return (false, false);
        }
        let sim =
            ScreenLayoutHasher::jaccard_similarity(&fingerprint.tokens, &expected.semantic_tokens);
        (sim >= threshold, false)
    }
}
