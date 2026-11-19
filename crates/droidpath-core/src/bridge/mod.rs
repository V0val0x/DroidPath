use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::protocol::{ShadowCompactNode, ShadowTreeMatchMode, ShadowUINode, WireSecureScreenPayload};
use crate::router::{
    AStarCostConfiguration, AStarPathfinder, DismissCandidate, HazardousConfirmationRequest,
    MacroActionExecutor, MacroExecutionConfiguration, MacroExecutionEngine, MacroExecutionEvent,
    MacroExecutionReport, PathDeviationError, UniversalDismissDetector,
};
use crate::shadow_dom::{ShadowDOMUpdate, ShadowDomState};
use crate::spatial_graph::{
    AttributedUIAction, ScreenLayoutHasher, ScreenSemanticFingerprint, ScreenStateVertex,
    SpatialGraphEngine, SpatialGraphSnapshot,
};
use crate::transport::{
    BeforeReconnectHook, DaemonConnectionState, DaemonEndpointConfiguration, DaemonTcpClient,
};

pub type DaemonRecoveryHook =
    Arc<dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowNavigationOutcome {
    Arrived(MacroExecutionReport),
    AlreadyAtTarget {
        state_hash: u64,
        active_package: String,
    },
    RequiresConfirmation(HazardousConfirmationRequest),
    SecureScreenTimeout {
        reason: String,
        active_package: String,
        timeout_seconds: u64,
    },
    NoRoute {
        start_hash: u64,
        active_package: String,
        candidate_target_hashes: Vec<u64>,
        reachable_vertex_count: usize,
        known_vertex_count: usize,
    },
    TargetNotFound {
        query: String,
        known_vertex_count: usize,
    },
    StreamUnavailable,
}

#[derive(Debug, Clone)]
pub struct RouteExecuteOptions {
    pub target_hint: Option<String>,
    pub target_hash: Option<u64>,
    pub max_steps: usize,
    pub step_timeout_ms: u64,
    pub post_settle_quiescence_ms: u64,
    pub settle_similarity_threshold: f64,
    pub confirmed: bool,
    pub enable_auto_recovery: bool,
    pub secure_handoff_timeout_secs: f64,
}

impl Default for RouteExecuteOptions {
    fn default() -> Self {
        Self {
            target_hint: None,
            target_hash: None,
            max_steps: 12,
            step_timeout_ms: 2000,
            post_settle_quiescence_ms: 40,
            settle_similarity_threshold: 0.80,
            confirmed: false,
            enable_auto_recovery: true,
            secure_handoff_timeout_secs: 60.0,
        }
    }
}

struct LifecycleState {
    is_started: bool,
    pump_tasks: Vec<JoinHandle<()>>,
}

/// Coordinates `DaemonTcpClient -> ShadowDomState -> SpatialGraphEngine` and Layer 3 A* routing.
pub struct ShadowGraphBridge {
    pub client: Arc<DaemonTcpClient>,
    pub dom_state: Arc<ShadowDomState>,
    pub graph_engine: Arc<SpatialGraphEngine>,
    pub hasher: ScreenLayoutHasher,

    navigation_active: AtomicBool,
    lifecycle: Mutex<LifecycleState>,
    daemon_recovery_hook: std::sync::Mutex<Option<DaemonRecoveryHook>>,
    macro_event_tx: broadcast::Sender<MacroExecutionEvent>,
}

impl ShadowGraphBridge {
    pub fn new(
        configuration: DaemonEndpointConfiguration,
        hasher: ScreenLayoutHasher,
        significance_threshold: f64,
    ) -> Self {
        let client = Arc::new(DaemonTcpClient::new(configuration));
        let client_for_resync = Arc::clone(&client);
        let dom_state = Arc::new(ShadowDomState::new(
            0.25,
            Some(Arc::new(move || {
                let c = Arc::clone(&client_for_resync);
                Box::pin(async move {
                    let _ = c.request_snapshot().await;
                })
            })),
        ));
        let graph_engine = Arc::new(SpatialGraphEngine::new(
            hasher.clone(),
            significance_threshold,
            5.0,
        ));
        let (macro_event_tx, _) = broadcast::channel(128);
        Self {
            client,
            dom_state,
            graph_engine,
            hasher,
            navigation_active: AtomicBool::new(false),
            lifecycle: Mutex::new(LifecycleState {
                is_started: false,
                pump_tasks: Vec::new(),
            }),
            daemon_recovery_hook: std::sync::Mutex::new(None),
            macro_event_tx,
        }
    }

    pub fn with_default_config() -> Self {
        Self::new(
            DaemonEndpointConfiguration::default(),
            ScreenLayoutHasher::default(),
            0.72,
        )
    }

    pub fn subscribe_macro_events(&self) -> broadcast::Receiver<MacroExecutionEvent> {
        self.macro_event_tx.subscribe()
    }

    pub fn subscribe_secure_screen_events(&self) -> broadcast::Receiver<WireSecureScreenPayload> {
        self.dom_state.subscribe_secure_screen()
    }

    fn emit_macro_event(&self, event: MacroExecutionEvent) {
        let _ = self.macro_event_tx.send(event);
    }

    pub async fn set_before_reconnect_hook(&self, hook: Option<BeforeReconnectHook>) {
        self.client.set_before_reconnect_hook(hook).await;
    }

    pub fn set_daemon_recovery_hook(&self, hook: Option<DaemonRecoveryHook>) {
        if let Ok(mut guard) = self.daemon_recovery_hook.lock() {
            *guard = hook;
        }
    }

    pub async fn start(&self) {
        {
            let mut lc = self.lifecycle.lock().await;
            if lc.is_started {
                return;
            }
            lc.is_started = true;

            let mut msg_rx = self.client.subscribe_messages();
            let mut state_rx = self.client.subscribe_states();
            let mut dom_rx = self.dom_state.subscribe_updates();

            let dom_for_msg = Arc::clone(&self.dom_state);
            let msg_task = tokio::spawn(async move {
                loop {
                    match msg_rx.recv().await {
                        Ok(msg) => dom_for_msg.handle_message(msg).await,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let dom_for_state = Arc::clone(&self.dom_state);
            let client_for_state = Arc::clone(&self.client);
            let state_task = tokio::spawn(async move {
                loop {
                    match state_rx.recv().await {
                        Ok(conn_state) => match conn_state {
                            DaemonConnectionState::Disconnected
                            | DaemonConnectionState::WaitingToReconnect { .. }
                            | DaemonConnectionState::Stopped => {
                                let current = client_for_state.state().await;
                                if matches!(
                                    current,
                                    DaemonConnectionState::Connecting
                                        | DaemonConnectionState::Connected { .. }
                                ) {
                                    continue;
                                }
                                dom_for_state.mark_disconnected().await;
                            }
                            DaemonConnectionState::Connecting
                            | DaemonConnectionState::Connected { .. } => {}
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            let graph_for_dom = Arc::clone(&self.graph_engine);
            let graph_task = tokio::spawn(async move {
                loop {
                    match dom_rx.recv().await {
                        Ok(upd) => {
                            let _ = graph_for_dom.process(&upd).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            lc.pump_tasks = vec![msg_task, state_task, graph_task];
        }

        self.client.start().await;
    }

    pub async fn stop(&self) {
        let tasks = {
            let mut lc = self.lifecycle.lock().await;
            lc.is_started = false;
            std::mem::take(&mut lc.pump_tasks)
        };
        for task in tasks {
            task.abort();
        }
        self.client.stop().await;
        self.dom_state.mark_disconnected().await;
    }

    pub async fn reconnect(&self) {
        self.dom_state.mark_disconnected().await;
        self.client.reconnect().await;
    }

    pub async fn get_current_tree(&self) -> Vec<ShadowUINode> {
        self.dom_state.get_current_tree().await
    }

    pub async fn get_compact_tree(&self) -> Vec<ShadowCompactNode> {
        self.dom_state.get_compact_tree().await
    }

    pub async fn ensure_shadow_stream_ready(
        &self,
        max_wait_secs: f64,
        max_age_secs: f64,
    ) -> bool {
        let is_started = self.lifecycle.lock().await.is_started;
        let conn_state = self.client.state().await;
        if is_started {
            match conn_state {
                DaemonConnectionState::Disconnected
                | DaemonConnectionState::WaitingToReconnect { .. }
                | DaemonConnectionState::Stopped => return false,
                DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. } => {}
            }
        }
        if self.dom_state.is_fresh(max_age_secs).await {
            return true;
        }
        match conn_state {
            DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. } => {
                if max_wait_secs > 0.0 {
                    let _ = self.dom_state.wait_for_snapshot(max_wait_secs).await;
                    return self.dom_state.is_fresh(max_age_secs).await;
                }
                false
            }
            _ => false,
        }
    }

    pub async fn fast_compact_tree_if_fresh(
        &self,
        max_wait_secs: f64,
        max_age_secs: f64,
    ) -> Option<Vec<ShadowCompactNode>> {
        if self.dom_state.is_fresh(max_age_secs).await {
            let compact = self.dom_state.get_compact_tree().await;
            if !compact.is_empty() {
                return Some(compact);
            }
        }

        let conn_state = self.client.state().await;
        match conn_state {
            DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. } => {
                if max_wait_secs > 0.0 {
                    let _ = self.dom_state.wait_for_snapshot(max_wait_secs).await;
                    if self.dom_state.is_fresh(max_age_secs).await {
                        let compact = self.dom_state.get_compact_tree().await;
                        if !compact.is_empty() {
                            return Some(compact);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    pub async fn find_node_by_text(
        &self,
        query: &str,
        mode: ShadowTreeMatchMode,
        prefer_clickable: bool,
    ) -> Option<ShadowUINode> {
        self.dom_state
            .find_node_by_text(query, mode, prefer_clickable)
            .await
    }

    pub async fn find_node_by_resource_id(
        &self,
        resource_id: &str,
        mode: ShadowTreeMatchMode,
    ) -> Option<ShadowUINode> {
        self.dom_state
            .find_node_by_resource_id(resource_id, mode)
            .await
    }

    pub async fn find_node_by_content_desc(
        &self,
        query: &str,
        mode: ShadowTreeMatchMode,
    ) -> Option<ShadowUINode> {
        self.dom_state
            .find_node_by_content_desc(query, mode)
            .await
    }

    pub async fn record_action(&self, action: AttributedUIAction) {
        self.graph_engine.record_pending_action(action).await;
    }

    pub async fn route_and_execute(
        &self,
        serial: &str,
        executor: Arc<dyn MacroActionExecutor>,
        opts: RouteExecuteOptions,
    ) -> Result<ShadowNavigationOutcome, PathDeviationError> {
        if self
            .navigation_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(PathDeviationError::ExecutionAlreadyInProgress);
        }
        struct NavGuard<'a>(&'a AtomicBool);
        impl Drop for NavGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = NavGuard(&self.navigation_active);

        self.perform_route_and_execute(serial, executor, opts).await
    }

    async fn perform_route_and_execute(
        &self,
        serial: &str,
        executor: Arc<dyn MacroActionExecutor>,
        opts: RouteExecuteOptions,
    ) -> Result<ShadowNavigationOutcome, PathDeviationError> {
        let mut stream_ready = self.ensure_shadow_stream_ready(0.75, 15.0).await;
        let mut resolved_index = if stream_ready {
            self.dom_state.current_index().await
        } else {
            None
        };

        if !stream_ready || resolved_index.is_none() {
            let hook = self
                .daemon_recovery_hook
                .lock()
                .ok()
                .and_then(|g| g.clone());
            if let Some(hook) = hook {
                let recovered = hook(serial.to_string(), self.client.configuration().port).await;
                if recovered {
                    stream_ready = self.ensure_shadow_stream_ready(0.75, 15.0).await;
                    resolved_index = if stream_ready {
                        self.dom_state.current_index().await
                    } else {
                        None
                    };
                }
            }
        }

        if !stream_ready || resolved_index.is_none() {
            self.emit_macro_event(MacroExecutionEvent::Failed {
                message: "Shadow DOM stream unavailable".to_string(),
            });
            return Ok(ShadowNavigationOutcome::StreamUnavailable);
        }

        let max_recovery_attempts = if opts.enable_auto_recovery { 2 } else { 0 };
        let mut recovery_attempt = 0usize;
        let mut dismissed_node_ids = HashSet::new();
        let mut accumulated_recovery: Vec<String> = Vec::new();
        let mut overall_start_hash: Option<u64> = None;
        let overall_start_time = Instant::now();

        loop {
            // Check FLAG_SECURE / biometric before planning (#7)
            let (is_secure, sec_reason, sec_pkg) = self.dom_state.secure_screen_info().await;
            if is_secure {
                let reason = sec_reason.unwrap_or_else(|| "biometric_or_pin".to_string());
                self.emit_macro_event(MacroExecutionEvent::AwaitingBiometrics {
                    reason: reason.clone(),
                    active_package: sec_pkg.clone(),
                });
                let cleared = self
                    .dom_state
                    .wait_until_secure_screen_cleared(opts.secure_handoff_timeout_secs)
                    .await;
                if !cleared {
                    let timeout_sec = opts.secure_handoff_timeout_secs.round() as u64;
                    self.emit_macro_event(MacroExecutionEvent::Failed {
                        message: format!("Secure screen remained locked past {timeout_sec}s"),
                    });
                    return Ok(ShadowNavigationOutcome::SecureScreenTimeout {
                        reason,
                        active_package: sec_pkg,
                        timeout_seconds: timeout_sec,
                    });
                }
                accumulated_recovery.push("secure_handoff".to_string());
                self.emit_macro_event(MacroExecutionEvent::BiometricsCleared);
            }

            let Some(current_index) = self.dom_state.current_index().await else {
                self.emit_macro_event(MacroExecutionEvent::Failed {
                    message: "Shadow DOM stream unavailable".to_string(),
                });
                return Ok(ShadowNavigationOutcome::StreamUnavailable);
            };

            let current_fp = self.hasher.fingerprint(&current_index);
            let mut start_hash = current_fp.layout_hash;
            if overall_start_hash.is_none() {
                overall_start_hash = Some(start_hash);
            }

            // Ensure live screen vertex exists in graphEngine before snapshotting
            if self.graph_engine.vertex(start_hash).await.is_none() {
                let _ = self
                    .graph_engine
                    .process(&ShadowDOMUpdate {
                        index: Arc::clone(&current_index),
                        is_full_snapshot: true,
                        inserted_count: current_index.ordered_nodes.len(),
                        updated_count: 0,
                        removed_count: 0,
                        event_type: None,
                        event_package: None,
                        is_secure_screen_active: false,
                    })
                    .await;
                if let Some(canonical_start) = self.graph_engine.current_state_hash().await {
                    if let Some(canonical_v) = self.graph_engine.vertex(canonical_start).await {
                        let sim_ok = canonical_start == start_hash
                            || (!canonical_v.semantic_tokens.is_empty()
                                && ScreenLayoutHasher::jaccard_similarity(
                                    &current_fp.tokens,
                                    &canonical_v.semantic_tokens,
                                ) >= opts.settle_similarity_threshold);
                        if canonical_v.active_package == current_fp.active_package && sim_ok {
                            start_hash = canonical_start;
                        }
                    }
                }
            }

            let snapshot = self.graph_engine.snapshot().await;
            let (top_goals, all_goals, early_outcome) = self
                .resolve_goals(opts.target_hint.as_deref(), opts.target_hash, &snapshot)
                .await;
            if let Some(early) = early_outcome {
                return Ok(early);
            }

            let pathfinder =
                AStarPathfinder::new(AStarCostConfiguration::with_max_steps(opts.max_steps));
            let mut search_result =
                pathfinder.find_shortest_path_to_any(start_hash, &top_goals, &snapshot);

            if search_result.is_none() && !current_fp.tokens.is_empty() {
                let aliases = Self::semantic_aliases(
                    &current_fp,
                    start_hash,
                    &snapshot.vertices,
                    opts.settle_similarity_threshold,
                );
                for (alias_hash, _) in &aliases {
                    if let Some(route) =
                        pathfinder.find_shortest_path_to_any(*alias_hash, &top_goals, &snapshot)
                    {
                        search_result = Some(route);
                        break;
                    }
                }
                if search_result.is_none() && all_goals != top_goals {
                    let mut fallback_goals = all_goals.clone();
                    fallback_goals.remove(&start_hash);
                    if !fallback_goals.is_empty() {
                        search_result = pathfinder.find_shortest_path_to_any(
                            start_hash,
                            &fallback_goals,
                            &snapshot,
                        );
                        if search_result.is_none() {
                            for (alias_hash, _) in &aliases {
                                let mut alias_fallback = fallback_goals.clone();
                                alias_fallback.remove(alias_hash);
                                if !alias_fallback.is_empty() {
                                    if let Some(route) = pathfinder.find_shortest_path_to_any(
                                        *alias_hash,
                                        &alias_fallback,
                                        &snapshot,
                                    ) {
                                        search_result = Some(route);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            } else if search_result.is_none() && all_goals != top_goals {
                let mut fallback_goals = all_goals.clone();
                fallback_goals.remove(&start_hash);
                if !fallback_goals.is_empty() {
                    search_result = pathfinder.find_shortest_path_to_any(
                        start_hash,
                        &fallback_goals,
                        &snapshot,
                    );
                }
            }

            // Universal Dismiss Auto-Recovery on no_route (#4)
            let Some(route) = search_result else {
                if opts.enable_auto_recovery && recovery_attempt < max_recovery_attempts {
                    if let Some(dismiss) = UniversalDismissDetector::find_dismiss_candidate(
                        &current_index,
                        &dismissed_node_ids,
                        &self.hasher.ignored_packages,
                    ) {
                        dismissed_node_ids.insert(dismiss.node_id);
                        recovery_attempt += 1;
                        accumulated_recovery.push(format!("universal_dismiss:{}", dismiss.label));
                        let dismissed = self
                            .execute_universal_dismiss(
                                &dismiss,
                                current_fp.layout_hash,
                                serial,
                                &executor,
                            )
                            .await?;
                        if dismissed {
                            continue;
                        }
                    }
                }

                let reachable = pathfinder.reachable_executable_vertex_count(start_hash, &snapshot);
                let known = snapshot.vertices.len();
                let target_label = if let Some(hint) = opts
                    .target_hint
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    format!("'{hint}'")
                } else if let Some(th) = opts.target_hash {
                    format!("0x{th:016x}")
                } else {
                    "target".to_string()
                };
                self.emit_macro_event(MacroExecutionEvent::Failed {
                    message: format!("No route to {target_label} ({reachable}/{known} reachable)"),
                });
                let mut sorted_candidates: Vec<u64> = all_goals.into_iter().collect();
                sorted_candidates.sort_unstable();
                return Ok(ShadowNavigationOutcome::NoRoute {
                    start_hash,
                    active_package: current_index.active_package.clone(),
                    candidate_target_hashes: sorted_candidates,
                    reachable_vertex_count: reachable,
                    known_vertex_count: known,
                });
            };

            if route.edges.is_empty() {
                if !accumulated_recovery.is_empty() {
                    let report = MacroExecutionReport {
                        start_hash: overall_start_hash.unwrap_or(start_hash),
                        final_hash: start_hash,
                        steps: Vec::new(),
                        total_elapsed_ms: overall_start_time.elapsed().as_millis() as u64,
                        recovered_steps: accumulated_recovery,
                    };
                    self.emit_macro_event(MacroExecutionEvent::Completed {
                        report: report.clone(),
                    });
                    return Ok(ShadowNavigationOutcome::Arrived(report));
                }
                return Ok(ShadowNavigationOutcome::AlreadyAtTarget {
                    state_hash: start_hash,
                    active_package: current_index.active_package.clone(),
                });
            }

            self.emit_macro_event(MacroExecutionEvent::PathComputed {
                total_steps: route.edges.len(),
                target_hint: opts.target_hint.clone(),
            });

            let tx_clone = self.macro_event_tx.clone();
            let engine = MacroExecutionEngine::new(
                Arc::clone(&self.dom_state),
                Arc::clone(&self.graph_engine),
                Arc::clone(&executor),
                self.hasher.clone(),
                MacroExecutionConfiguration {
                    step_timeout_ms: opts.step_timeout_ms,
                    post_settle_quiescence_ms: opts.post_settle_quiescence_ms,
                    settle_similarity_threshold: opts.settle_similarity_threshold,
                    allow_hazardous_actions: opts.confirmed,
                    enable_forward_skip_recovery: opts.enable_auto_recovery,
                    enable_overlay_swipe_dismiss: opts.enable_auto_recovery,
                    secure_handoff_timeout_secs: opts.secure_handoff_timeout_secs,
                },
                Some(Arc::new(move |ev| {
                    let _ = tx_clone.send(ev);
                })),
            );

            match engine.execute(&route.edges, serial).await {
                Ok(mut report) => {
                    if !accumulated_recovery.is_empty() {
                        let mut combined = accumulated_recovery;
                        combined.extend(report.recovered_steps);
                        report.start_hash = overall_start_hash.unwrap_or(report.start_hash);
                        report.total_elapsed_ms = overall_start_time.elapsed().as_millis() as u64;
                        report.recovered_steps = combined;
                    }
                    return Ok(ShadowNavigationOutcome::Arrived(report));
                }
                Err(PathDeviationError::RequiresConfirmation(req)) => {
                    return Ok(ShadowNavigationOutcome::RequiresConfirmation(req));
                }
                Err(PathDeviationError::SecureScreenTimeout {
                    reason,
                    active_package,
                    timeout_seconds,
                }) => {
                    return Ok(ShadowNavigationOutcome::SecureScreenTimeout {
                        reason,
                        active_package,
                        timeout_seconds,
                    });
                }
                Err(PathDeviationError::UnexpectedStartState {
                    expected_hash,
                    actual_hash,
                    actual_package,
                }) => {
                    if opts.enable_auto_recovery && recovery_attempt < max_recovery_attempts {
                        if let Some(live_index) = self.dom_state.current_index().await {
                            let live_fp = self.hasher.fingerprint(&live_index);
                            let refreshed = self.graph_engine.snapshot().await;
                            if live_fp.layout_hash != start_hash
                                && Self::has_executable_route(
                                    &live_fp,
                                    &top_goals,
                                    &refreshed,
                                    &pathfinder,
                                    opts.settle_similarity_threshold,
                                )
                            {
                                recovery_attempt += 1;
                                accumulated_recovery.push("replan_unexpected_start".to_string());
                                self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
                                    kind: "replan".to_string(),
                                    detail: "Re-planning A* after start-state shift".to_string(),
                                });
                                continue;
                            }
                        }
                    }
                    return Err(PathDeviationError::UnexpectedStartState {
                        expected_hash,
                        actual_hash,
                        actual_package,
                    });
                }
                Err(PathDeviationError::StepTimeout {
                    step_index,
                    expected_target_hash,
                    last_observed_hash,
                    last_observed_package,
                    timeout_ms,
                }) => {
                    if opts.enable_auto_recovery && recovery_attempt < max_recovery_attempts {
                        if let Some(live_index) = self.dom_state.current_index().await {
                            let live_fp = self.hasher.fingerprint(&live_index);
                            let refreshed = self.graph_engine.snapshot().await;

                            // 1. Already semantically at goal?
                            if Self::matches_any_goal(
                                &live_fp,
                                &top_goals,
                                &refreshed,
                                opts.settle_similarity_threshold,
                            ) {
                                let mut rec = accumulated_recovery;
                                rec.push("arrived_on_recheck".to_string());
                                let report = MacroExecutionReport {
                                    start_hash: overall_start_hash.unwrap_or(live_fp.layout_hash),
                                    final_hash: live_fp.layout_hash,
                                    steps: Vec::new(),
                                    total_elapsed_ms: overall_start_time.elapsed().as_millis()
                                        as u64,
                                    recovered_steps: rec,
                                };
                                self.emit_macro_event(MacroExecutionEvent::Completed {
                                    report: report.clone(),
                                });
                                return Ok(ShadowNavigationOutcome::Arrived(report));
                            }

                            // 2. Mid-route popup dismiss (#4)
                            if let Some(dismiss) = UniversalDismissDetector::find_dismiss_candidate(
                                &live_index,
                                &dismissed_node_ids,
                                &self.hasher.ignored_packages,
                            ) {
                                dismissed_node_ids.insert(dismiss.node_id);
                                recovery_attempt += 1;
                                accumulated_recovery
                                    .push(format!("universal_dismiss:{}", dismiss.label));
                                if self
                                    .execute_universal_dismiss(
                                        &dismiss,
                                        live_fp.layout_hash,
                                        serial,
                                        &executor,
                                    )
                                    .await?
                                {
                                    continue;
                                }
                            }

                            // 3. Re-plan from live screen / semantic alias (#1)
                            if live_fp.layout_hash != start_hash
                                && Self::has_executable_route(
                                    &live_fp,
                                    &top_goals,
                                    &refreshed,
                                    &pathfinder,
                                    opts.settle_similarity_threshold,
                                )
                            {
                                recovery_attempt += 1;
                                accumulated_recovery.push(format!(
                                    "replan_from:0x{:016x}",
                                    live_fp.layout_hash
                                ));
                                self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
                                    kind: "replan".to_string(),
                                    detail: "Re-planning A* from live screen".to_string(),
                                });
                                continue;
                            }
                        }
                    }

                    return Err(PathDeviationError::StepTimeout {
                        step_index,
                        expected_target_hash,
                        last_observed_hash,
                        last_observed_package,
                        timeout_ms,
                    });
                }
                Err(other) => return Err(other),
            }
        }
    }

    async fn resolve_goals(
        &self,
        target_hint: Option<&str>,
        target_hash: Option<u64>,
        snapshot: &SpatialGraphSnapshot,
    ) -> (HashSet<u64>, HashSet<u64>, Option<ShadowNavigationOutcome>) {
        if let Some(th) = target_hash {
            let hex = format!("0x{th:016x}");
            if !snapshot.vertices.iter().any(|v| v.layout_hash == th) {
                let count = snapshot.vertices.len();
                self.emit_macro_event(MacroExecutionEvent::Failed {
                    message: format!("Target {hex} not found ({count} screens)"),
                });
                return (
                    HashSet::new(),
                    HashSet::new(),
                    Some(ShadowNavigationOutcome::TargetNotFound {
                        query: hex,
                        known_vertex_count: count,
                    }),
                );
            }
            let mut set = HashSet::new();
            set.insert(th);
            return (set.clone(), set, None);
        }

        if let Some(trimmed) = target_hint.map(str::trim).filter(|s| !s.is_empty()) {
            let ranked = self
                .graph_engine
                .resolve_target_vertices(trimmed, 12)
                .await;
            let Some(best) = ranked.first() else {
                let count = snapshot.vertices.len();
                self.emit_macro_event(MacroExecutionEvent::Failed {
                    message: format!("Target '{trimmed}' not found ({count} screens)"),
                });
                return (
                    HashSet::new(),
                    HashSet::new(),
                    Some(ShadowNavigationOutcome::TargetNotFound {
                        query: trimmed.to_string(),
                        known_vertex_count: count,
                    }),
                );
            };
            let best_score = best.score;
            let top: HashSet<u64> = ranked
                .iter()
                .filter(|r| r.score == best_score)
                .map(|r| r.vertex.layout_hash)
                .collect();
            let all: HashSet<u64> = ranked.iter().map(|r| r.vertex.layout_hash).collect();
            return (top, all, None);
        }

        let count = snapshot.vertices.len();
        self.emit_macro_event(MacroExecutionEvent::Failed {
            message: format!("Empty navigation target ({count} screens)"),
        });
        (
            HashSet::new(),
            HashSet::new(),
            Some(ShadowNavigationOutcome::TargetNotFound {
                query: String::new(),
                known_vertex_count: count,
            }),
        )
    }

    async fn execute_universal_dismiss(
        &self,
        candidate: &DismissCandidate,
        previous_hash: u64,
        serial: &str,
        executor: &Arc<dyn MacroActionExecutor>,
    ) -> Result<bool, PathDeviationError> {
        self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
            kind: "universal_dismiss".to_string(),
            detail: format!("Dismissing popup '{}'", candidate.label),
        });

        let mut update_rx = self.dom_state.subscribe_updates();
        let tap_action = AttributedUIAction::tap(
            format!("Dismiss '{}'", candidate.label),
            Some(candidate.tap_x),
            Some(candidate.tap_y),
        );
        // Do NOT call graph_engine.record_pending_action so popup dismissal never pollutes graph
        executor.execute(&tap_action, serial).await?;

        let deadline = tokio::time::sleep(Duration::from_millis(1200));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Ok(false),
                res = update_rx.recv() => {
                    match res {
                        Ok(upd) => {
                            let new_hash = self.hasher.fingerprint(&upd.index).layout_hash;
                            if new_hash != previous_hash || !upd.index.nodes_by_id.contains_key(&candidate.node_id) {
                                return Ok(true);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return Ok(false),
                    }
                }
            }
        }
    }

    fn semantic_aliases(
        current_fp: &ScreenSemanticFingerprint,
        start_hash: u64,
        vertices: &[ScreenStateVertex],
        threshold: f64,
    ) -> Vec<(u64, f64)> {
        let mut aliases: Vec<(u64, f64)> = vertices
            .iter()
            .filter_map(|v| {
                if v.layout_hash == start_hash
                    || v.active_package != current_fp.active_package
                    || v.semantic_tokens.is_empty()
                {
                    return None;
                }
                let sim =
                    ScreenLayoutHasher::jaccard_similarity(&current_fp.tokens, &v.semantic_tokens);
                (sim >= threshold).then_some((v.layout_hash, sim))
            })
            .collect();
        aliases.sort_by(|lhs, rhs| rhs.1.total_cmp(&lhs.1).then_with(|| lhs.0.cmp(&rhs.0)));
        aliases
    }

    fn matches_any_goal(
        fingerprint: &ScreenSemanticFingerprint,
        goals: &HashSet<u64>,
        snapshot: &SpatialGraphSnapshot,
        threshold: f64,
    ) -> bool {
        if goals.contains(&fingerprint.layout_hash) {
            return true;
        }
        for vertex in &snapshot.vertices {
            if !goals.contains(&vertex.layout_hash)
                || vertex.active_package != fingerprint.active_package
                || vertex.semantic_tokens.is_empty()
            {
                continue;
            }
            if ScreenLayoutHasher::jaccard_similarity(&fingerprint.tokens, &vertex.semantic_tokens)
                >= threshold
            {
                return true;
            }
        }
        false
    }

    fn has_executable_route(
        fp: &ScreenSemanticFingerprint,
        goals: &HashSet<u64>,
        snapshot: &SpatialGraphSnapshot,
        pathfinder: &AStarPathfinder,
        threshold: f64,
    ) -> bool {
        if pathfinder
            .find_shortest_path_to_any(fp.layout_hash, goals, snapshot)
            .is_some()
        {
            return true;
        }
        for (alias_hash, _) in Self::semantic_aliases(fp, fp.layout_hash, &snapshot.vertices, threshold)
        {
            if pathfinder
                .find_shortest_path_to_any(alias_hash, goals, snapshot)
                .is_some()
            {
                return true;
            }
        }
        false
    }
}
