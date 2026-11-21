use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::protocol::{ShadowCompactNode, ShadowTreeMatchMode, ShadowUINode, WireSecureScreenPayload};
use crate::router::{
    AStarCostConfiguration, AStarPathfinder, DismissCandidate, HazardousConfirmationRequest,
    MacroActionExecutor, MacroExecutionConfiguration, MacroExecutionEngine, MacroExecutionEvent,
    MacroExecutionReport, PathDeviationError, UniversalDismissDetector,
};
use crate::shadow_dom::{ShadowDOMUpdate, ShadowDomState, ShadowTreeIndex};
use crate::spatial_graph::{
    AttributedUIAction, HazardCategory, HazardousActionClassifier, ScreenLayoutHasher,
    ScreenSemanticFingerprint, ScreenStateVertex, ScreenTransitionEdge, SeedGraphCatalog,
    SpatialGraphEngine, SpatialGraphSnapshot,
};
use crate::transport::{
    BeforeReconnectHook, DaemonConnectionState, DaemonEndpointConfiguration, DaemonTcpClient,
};

pub type DaemonRecoveryHook =
    Arc<dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreCandidate {
    pub node_id: i64,
    pub label: String,
    pub resource_id: String,
    pub class_name: String,
    pub center_x: i32,
    pub center_y: i32,
    pub clickable: bool,
    pub scrollable: bool,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExploreOutcome {
    ArrivedViaGraph {
        report: MacroExecutionReport,
        already_at_target: bool,
        active_package: String,
    },
    ExploredAndClicked {
        matched_candidate: ExploreCandidate,
        scrolls_performed: usize,
        start_hash: u64,
        final_hash: u64,
        active_package: String,
        screen_changed: bool,
        edge_recorded: bool,
        total_elapsed_ms: u64,
    },
    RequiresConfirmation(HazardousConfirmationRequest),
    SecureScreenTimeout {
        reason: String,
        active_package: String,
        timeout_seconds: u64,
    },
    NeedsSelection {
        query: String,
        current_hash: u64,
        active_package: String,
        scrolls_performed: usize,
        candidates: Vec<ExploreCandidate>,
    },
}

#[derive(Debug, Clone)]
pub struct ExploreOptions {
    pub max_scrolls: usize,
    pub step_timeout_ms: u64,
    pub post_settle_quiescence_ms: u64,
    pub settle_similarity_threshold: f64,
    pub confirmed: bool,
    pub prefer_graph_route: bool,
    pub secure_handoff_timeout_secs: f64,
}

impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            max_scrolls: 3,
            step_timeout_ms: 2000,
            post_settle_quiescence_ms: 40,
            settle_similarity_threshold: 0.80,
            confirmed: false,
            prefer_graph_route: true,
            secure_handoff_timeout_secs: 60.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowNavigationOutcome {
    Arrived(MacroExecutionReport),
    AlreadyAtTarget {
        state_hash: u64,
        active_package: String,
    },
    Explored(ExploreOutcome),
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
    pub auto_explore: bool,
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
            auto_explore: false,
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
                if opts.auto_explore {
                    if let Some(target_str) = opts
                        .target_hint
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if matches!(early, ShadowNavigationOutcome::TargetNotFound { .. }) {
                            return self
                                .fallback_to_auto_explore(target_str, serial, executor, &opts)
                                .await;
                        }
                    }
                }
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

            // Cross-package Zero-PII Seed Graph entry bridge:
            // When starting outside `com.android.settings` (e.g. Home launcher on a brand-new phone)
            // and routing to a seeded Android Settings screen, prepend a `LaunchApp("com.android.settings")`
            // edge into the Settings root so cold-start navigation succeeds in 1 call and hydrates the graph.
            if search_result.is_none()
                && current_fp.active_package != SeedGraphCatalog::ANDROID_SETTINGS_PACKAGE
                && opts.max_steps >= 1
            {
                search_result = Self::synthesize_settings_seed_entry_route(
                    start_hash,
                    &top_goals,
                    &all_goals,
                    &snapshot,
                    opts.max_steps,
                );
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

                if opts.auto_explore {
                    if let Some(target_str) = opts
                        .target_hint
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        return self
                            .fallback_to_auto_explore(target_str, serial, executor, &opts)
                            .await;
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
                if opts.auto_explore && opts.target_hash.is_none() {
                    if let Some(target_str) = opts
                        .target_hint
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if self.should_auto_explore_instead_of_already_at_target(
                            target_str,
                            start_hash,
                            &current_index,
                            &snapshot,
                        ) {
                            return self
                                .fallback_to_auto_explore(target_str, serial, executor, &opts)
                                .await;
                        }
                    }
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
                    enable_smart_scroll_to_selector: opts.enable_auto_recovery,
                    record_edge_failures_on_timeout: false,
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
                                if let Some(failed_edge) = route.edges.get(step_index) {
                                    self.graph_engine.record_edge_success(failed_edge.id).await;
                                }
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
                                    if let Some(failed_edge) = route.edges.get(step_index) {
                                        self.graph_engine.record_edge_success(failed_edge.id).await;
                                    }
                                    continue;
                                }
                            }

                            // Compute cross-package drift context up front so `replan_from` never
                            // loops back through a `DeepSubScreen --(HOME)--> Home -> failed_edge` cycle.
                            let failed_edge_opt = route.edges.get(step_index);
                            let cross_pkg_drift_ctx = failed_edge_opt.and_then(|fe| {
                                let from_pkg = snapshot
                                    .vertices
                                    .iter()
                                    .find(|v| v.layout_hash == fe.from_hash)
                                    .map(|v| v.active_package.as_str())
                                    .unwrap_or("");
                                let expected_to_pkg = snapshot
                                    .vertices
                                    .iter()
                                    .find(|v| v.layout_hash == fe.to_hash)
                                    .map(|v| v.active_package.as_str())
                                    .unwrap_or("");
                                if !expected_to_pkg.is_empty()
                                    && from_pkg != expected_to_pkg
                                    && live_fp.active_package == expected_to_pkg
                                {
                                    Some((fe, expected_to_pkg))
                                } else {
                                    None
                                }
                            });

                            // 3. Re-plan from live screen / semantic alias (#1)
                            if live_fp.layout_hash != start_hash
                                && Self::has_executable_route_avoiding_cycle(
                                    &live_fp,
                                    &top_goals,
                                    &refreshed,
                                    &pathfinder,
                                    opts.settle_similarity_threshold,
                                    failed_edge_opt.map(|e| e.id),
                                    cross_pkg_drift_ctx.map(|(fe, _)| fe.from_hash),
                                )
                            {
                                if let Some(failed_edge) = failed_edge_opt {
                                    self.graph_engine.record_edge_success(failed_edge.id).await;
                                }
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

                            // 4. App Task-Stack Drift (Back-to-Root):
                            // If a cross-package transition landed on a deep sub-screen inside the
                            // target package with no direct in-app route, issue up to 3 safe
                            // KEYCODE_BACK presses strictly within that package until a routable
                            // vertex appears.
                            if let Some((failed_edge, expected_to_pkg)) = cross_pkg_drift_ctx {
                                match self
                                    .recover_task_stack_drift_back_to_root(
                                        expected_to_pkg,
                                        live_fp.layout_hash,
                                        &top_goals,
                                        &refreshed,
                                        &pathfinder,
                                        opts.settle_similarity_threshold,
                                        opts.step_timeout_ms,
                                        opts.post_settle_quiescence_ms,
                                        failed_edge.id,
                                        failed_edge.from_hash,
                                        serial,
                                        &executor,
                                    )
                                    .await?
                                {
                                    BackToRootResult::ArrivedAtGoal {
                                        backs_used,
                                        final_hash,
                                    } => {
                                        self.graph_engine
                                            .record_edge_success(failed_edge.id)
                                            .await;
                                        let mut rec = accumulated_recovery;
                                        rec.push(format!("back_to_root:{backs_used}"));
                                        let report = MacroExecutionReport {
                                            start_hash: overall_start_hash.unwrap_or(final_hash),
                                            final_hash,
                                            steps: Vec::new(),
                                            total_elapsed_ms: overall_start_time
                                                .elapsed()
                                                .as_millis()
                                                as u64,
                                            recovered_steps: rec,
                                        };
                                        self.emit_macro_event(MacroExecutionEvent::Completed {
                                            report: report.clone(),
                                        });
                                        return Ok(ShadowNavigationOutcome::Arrived(report));
                                    }
                                    BackToRootResult::FoundRoute { backs_used } => {
                                        self.graph_engine
                                            .record_edge_success(failed_edge.id)
                                            .await;
                                        recovery_attempt += 1;
                                        accumulated_recovery
                                            .push(format!("back_to_root:{backs_used}"));
                                        continue;
                                    }
                                    BackToRootResult::NotRecovered => {}
                                }
                            }
                        }
                    }

                    if let Some(failed_edge) = route.edges.get(step_index) {
                        let _ = self.graph_engine.record_edge_failure(failed_edge.id).await;
                    }

                    return Err(PathDeviationError::StepTimeout {
                        step_index,
                        expected_target_hash,
                        last_observed_hash,
                        last_observed_package,
                        timeout_ms,
                    });
                }
                Err(PathDeviationError::ActionDispatchFailed {
                    step_index,
                    action_signature,
                    underlying_message,
                }) => {
                    let failed_on_seed = step_index >= 0
                        && route
                            .edges
                            .get(step_index as usize)
                            .is_some_and(|e| e.is_seed);
                    if failed_on_seed && opts.auto_explore {
                        if let Some(target_str) = opts
                            .target_hint
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            return self
                                .fallback_to_auto_explore(target_str, serial, executor, &opts)
                                .await;
                        }
                    }
                    return Err(PathDeviationError::ActionDispatchFailed {
                        step_index,
                        action_signature,
                        underlying_message,
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

    fn synthesize_settings_seed_entry_route(
        start_hash: u64,
        top_goals: &HashSet<u64>,
        all_goals: &HashSet<u64>,
        snapshot: &SpatialGraphSnapshot,
        max_steps: usize,
    ) -> Option<crate::router::astar::AStarSearchResult> {
        let has_settings_goal = snapshot.vertices.iter().any(|v| {
            v.active_package == SeedGraphCatalog::ANDROID_SETTINGS_PACKAGE
                && (top_goals.contains(&v.layout_hash) || all_goals.contains(&v.layout_hash))
        });
        if !has_settings_goal {
            return None;
        }
        let settings_root = snapshot
            .vertices
            .iter()
            .filter(|v| {
                v.active_package == SeedGraphCatalog::ANDROID_SETTINGS_PACKAGE
                    && ((v.is_seed && v.discovery_depth == 0)
                        || v.semantic_aliases
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case("com.android.settings")))
            })
            .min_by(|a, b| {
                a.is_seed
                    .cmp(&b.is_seed)
                    .then_with(|| a.discovery_depth.cmp(&b.discovery_depth))
                    .then_with(|| a.layout_hash.cmp(&b.layout_hash))
            })?;

        let remaining_steps = max_steps.saturating_sub(1);
        let launch_action = AttributedUIAction::launch_app(SeedGraphCatalog::ANDROID_SETTINGS_PACKAGE);
        let launch_sig = launch_action.signature();
        let entry_edge = ScreenTransitionEdge {
            id: uuid::Uuid::new_v4(),
            from_hash: start_hash,
            to_hash: settings_root.layout_hash,
            action: Some(launch_action),
            action_signature: launch_sig,
            traversal_count: 0,
            average_transition_ms: 450.0,
            is_spanning_tree_edge: false,
            last_traversed_at: chrono::Utc::now(),
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
            is_seed: true,
        };

        if top_goals.contains(&settings_root.layout_hash) {
            return Some(crate::router::astar::AStarSearchResult {
                start_hash,
                target_hash: settings_root.layout_hash,
                edges: vec![entry_edge],
                total_cost: 1.25,
                expanded_node_count: 1,
            });
        }
        if remaining_steps == 0 {
            return None;
        }

        let inner_pf = AStarPathfinder::new(AStarCostConfiguration::with_max_steps(remaining_steps));
        let inner_route = inner_pf
            .find_shortest_path_to_any(settings_root.layout_hash, top_goals, snapshot)
            .or_else(|| {
                if all_goals != top_goals {
                    inner_pf.find_shortest_path_to_any(
                        settings_root.layout_hash,
                        all_goals,
                        snapshot,
                    )
                } else {
                    None
                }
            })?;

        let mut combined_edges = Vec::with_capacity(inner_route.edges.len() + 1);
        combined_edges.push(entry_edge);
        combined_edges.extend(inner_route.edges);
        Some(crate::router::astar::AStarSearchResult {
            start_hash,
            target_hash: inner_route.target_hash,
            edges: combined_edges,
            total_cost: inner_route.total_cost + 1.25,
            expanded_node_count: inner_route.expanded_node_count + 1,
        })
    }

    fn semantic_aliases(
        current_fp: &ScreenSemanticFingerprint,
        start_hash: u64,
        vertices: &[ScreenStateVertex],
        threshold: f64,
    ) -> Vec<(u64, f64)> {
        let mut aliases: Vec<(u64, f64, bool)> = vertices
            .iter()
            .filter_map(|v| {
                if v.layout_hash == start_hash || v.active_package != current_fp.active_package {
                    return None;
                }
                if v.is_seed {
                    let seed_score = SeedGraphCatalog::seed_match_score(current_fp, v);
                    return (seed_score >= SeedGraphCatalog::SEED_MATCH_THRESHOLD)
                        .then_some((v.layout_hash, seed_score, true));
                }
                if v.semantic_tokens.is_empty() {
                    return None;
                }
                let sim =
                    ScreenLayoutHasher::jaccard_similarity(&current_fp.tokens, &v.semantic_tokens);
                (sim >= threshold).then_some((v.layout_hash, sim, false))
            })
            .collect();
        // Prefer calibrated real semantic aliases ahead of unhydrated seed aliases; highest score first.
        aliases.sort_by(|lhs, rhs| {
            lhs.2
                .cmp(&rhs.2)
                .then_with(|| rhs.1.total_cmp(&lhs.1))
                .then_with(|| lhs.0.cmp(&rhs.0))
        });
        aliases.into_iter().map(|(h, s, _)| (h, s)).collect()
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
            {
                continue;
            }
            if vertex.is_seed {
                if SeedGraphCatalog::seed_match_score(fingerprint, vertex)
                    >= SeedGraphCatalog::SEED_MATCH_THRESHOLD
                {
                    return true;
                }
                continue;
            }
            if vertex.semantic_tokens.is_empty() {
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
        Self::has_executable_route_avoiding_cycle(
            fp, goals, snapshot, pathfinder, threshold, None, None,
        )
    }

    fn has_executable_route_avoiding_cycle(
        fp: &ScreenSemanticFingerprint,
        goals: &HashSet<u64>,
        snapshot: &SpatialGraphSnapshot,
        pathfinder: &AStarPathfinder,
        threshold: f64,
        excluded_edge_id: Option<uuid::Uuid>,
        excluded_vertex: Option<u64>,
    ) -> bool {
        let is_productive = |route: crate::router::astar::AStarSearchResult| -> bool {
            if let Some(eid) = excluded_edge_id {
                if route.edges.iter().any(|e| e.id == eid) {
                    return false;
                }
            }
            if let Some(vx) = excluded_vertex {
                if route
                    .edges
                    .iter()
                    .any(|e| e.from_hash == vx || e.to_hash == vx)
                {
                    return false;
                }
            }
            true
        };
        if pathfinder
            .find_shortest_path_to_any(fp.layout_hash, goals, snapshot)
            .is_some_and(is_productive)
        {
            return true;
        }
        for (alias_hash, _) in
            Self::semantic_aliases(fp, fp.layout_hash, &snapshot.vertices, threshold)
        {
            if pathfinder
                .find_shortest_path_to_any(alias_hash, goals, snapshot)
                .is_some_and(is_productive)
            {
                return true;
            }
        }
        false
    }

    // MARK: - App Task-Stack Drift (Back-to-Root)

    #[allow(clippy::too_many_arguments)]
    async fn recover_task_stack_drift_back_to_root(
        &self,
        expected_package: &str,
        initial_hash: u64,
        top_goals: &HashSet<u64>,
        graph_snapshot: &SpatialGraphSnapshot,
        pathfinder: &AStarPathfinder,
        settle_threshold: f64,
        step_timeout_ms: u64,
        post_settle_quiescence_ms: u64,
        failed_edge_id: uuid::Uuid,
        failed_from_hash: u64,
        serial: &str,
        executor: &Arc<dyn MacroActionExecutor>,
    ) -> Result<BackToRootResult, PathDeviationError> {
        let mut prev_hash = initial_hash;

        for back_attempt in 1..=3usize {
            self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
                kind: "back_to_root".to_string(),
                detail: format!(
                    "Task-stack drift in {expected_package}: pressing BACK ({back_attempt}/3)"
                ),
            });

            // Subscribe BEFORE BACK and do NOT record pending action so recovery never pollutes DAG.
            let mut update_rx = self.dom_state.subscribe_updates();
            let back_action = AttributedUIAction::keyevent("BACK");
            executor.execute(&back_action, serial).await?;

            let wait_ms = step_timeout_ms.clamp(200, 1200);
            let deadline = tokio::time::sleep(Duration::from_millis(wait_ms));
            tokio::pin!(deadline);
            let mut observed_after: Option<ScreenSemanticFingerprint> = None;

            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    res = update_rx.recv() => {
                        match res {
                            Ok(upd) => {
                                if upd.is_secure_screen_active {
                                    continue;
                                }
                                let fp = self.hasher.fingerprint(&upd.index);
                                if fp.tokens.is_empty()
                                    || Self::is_transient_ignored_system_frame(
                                        &upd.index,
                                        &fp.active_package,
                                        expected_package,
                                        &self.hasher.ignored_packages,
                                    )
                                {
                                    continue;
                                }
                                if fp.layout_hash != prev_hash || fp.active_package != expected_package {
                                    observed_after = Some(fp);
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }

            if post_settle_quiescence_ms > 0 && observed_after.is_some() {
                // Debounce Android Activity-pop transition frames (e.g. half-inflated Settings
                // root with only `Search Settings`) so we inspect the fully rendered parent screen
                // before deciding whether to press BACK again.
                let quiet_ms = post_settle_quiescence_ms.max(220);
                let max_cap = tokio::time::sleep(Duration::from_millis(quiet_ms.saturating_mul(3).max(650)));
                tokio::pin!(max_cap);
                let quiet_timer = tokio::time::sleep(Duration::from_millis(quiet_ms));
                tokio::pin!(quiet_timer);
                loop {
                    tokio::select! {
                        _ = &mut max_cap => break,
                        _ = &mut quiet_timer => break,
                        res = update_rx.recv() => {
                            match res {
                                Ok(upd) => {
                                    if upd.is_secure_screen_active {
                                        continue;
                                    }
                                    let fp = self.hasher.fingerprint(&upd.index);
                                    if fp.tokens.is_empty()
                                        || Self::is_transient_ignored_system_frame(
                                            &upd.index,
                                            &fp.active_package,
                                            expected_package,
                                            &self.hasher.ignored_packages,
                                        )
                                    {
                                        continue;
                                    }
                                    observed_after = Some(fp);
                                    quiet_timer.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(quiet_ms));
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                }
            }

            let Some(after_fp) = observed_after else {
                // Re-read current index in case update arrived right at deadline
                if let Some(idx) = self.dom_state.current_index().await {
                    let fp = self.hasher.fingerprint(&idx);
                    if fp.layout_hash == prev_hash {
                        break;
                    }
                    if fp.active_package != expected_package {
                        break;
                    }
                    if Self::matches_any_goal(&fp, top_goals, graph_snapshot, settle_threshold) {
                        return Ok(BackToRootResult::ArrivedAtGoal {
                            backs_used: back_attempt,
                            final_hash: fp.layout_hash,
                        });
                    }
                    if Self::has_executable_route_avoiding_cycle(
                        &fp,
                        top_goals,
                        graph_snapshot,
                        pathfinder,
                        settle_threshold,
                        Some(failed_edge_id),
                        Some(failed_from_hash),
                    ) {
                        return Ok(BackToRootResult::FoundRoute {
                            backs_used: back_attempt,
                        });
                    }
                    prev_hash = fp.layout_hash;
                    continue;
                }
                break;
            };

            if after_fp.active_package != expected_package {
                // Never issue further BACK presses once outside the target package.
                break;
            }

            if Self::matches_any_goal(&after_fp, top_goals, graph_snapshot, settle_threshold) {
                return Ok(BackToRootResult::ArrivedAtGoal {
                    backs_used: back_attempt,
                    final_hash: after_fp.layout_hash,
                });
            }

            if Self::has_executable_route_avoiding_cycle(
                &after_fp,
                top_goals,
                graph_snapshot,
                pathfinder,
                settle_threshold,
                Some(failed_edge_id),
                Some(failed_from_hash),
            ) {
                return Ok(BackToRootResult::FoundRoute {
                    backs_used: back_attempt,
                });
            }

            prev_hash = after_fp.layout_hash;
        }

        Ok(BackToRootResult::NotRecovered)
    }

    /// Matches `SpatialGraphEngine::process`'s transient status-bar / systemui transition filter.
    fn is_transient_ignored_system_frame(
        index: &ShadowTreeIndex,
        active_package: &str,
        previous_package: &str,
        ignored_packages: &HashSet<String>,
    ) -> bool {
        ignored_packages.contains(active_package)
            && !ignored_packages.contains(previous_package)
            && !index
                .ordered_nodes
                .iter()
                .any(|n| n.visible && (n.clickable || n.scrollable || n.editable))
    }

    // MARK: - Dynamic Fallback (`explore_and_click` & `auto_explore`)

    /// Detects the cold-start / unvisited-subscreen ambiguity where `resolve_target_vertices`
    /// matched the *current* screen solely because a clickable icon/row for `target` (e.g.
    /// "Settings" on Home launcher, or "Display" on Settings root) is in its
    /// `sample_interactive_labels`, while the destination screen itself is not yet in the DAG.
    fn should_auto_explore_instead_of_already_at_target(
        &self,
        target: &str,
        start_hash: u64,
        current_index: &ShadowTreeIndex,
        snapshot: &SpatialGraphSnapshot,
    ) -> bool {
        let q = target.trim().to_lowercase();
        if q.is_empty() {
            return false;
        }
        // Already inside a package matching the query (e.g. "Settings" in com.android.settings).
        if current_index.active_package.to_lowercase().contains(&q) {
            return false;
        }
        // An incoming edge already arrived at this vertex via an action matching the query.
        let arrived_via_matching_action = snapshot.edges.iter().any(|edge| {
            if edge.to_hash != start_hash {
                return false;
            }
            let Some(ref action) = edge.action else {
                return false;
            };
            [
                Some(action.target_description.as_str()),
                action.selector_text.as_deref(),
                action.selector_content_desc.as_deref(),
                action.selector_resource_id.as_deref(),
            ]
            .into_iter()
            .flatten()
            .chain(action.selector_aliases.iter().map(String::as_str))
            .any(|raw| {
                let lower = raw.trim().to_lowercase();
                !lower.is_empty() && (lower == q || lower.contains(&q))
            })
        });
        if arrived_via_matching_action {
            return false;
        }
        // Only hand off to auto_explore when a confident live clickable trigger exists right here.
        Self::score_live_candidates(current_index, target, &self.hasher.ignored_packages)
            .first()
            .is_some_and(|c| c.score >= 500 && c.clickable)
    }

    async fn fallback_to_auto_explore(
        &self,
        target: &str,
        serial: &str,
        executor: Arc<dyn MacroActionExecutor>,
        opts: &RouteExecuteOptions,
    ) -> Result<ShadowNavigationOutcome, PathDeviationError> {
        let explore_opts = ExploreOptions {
            max_scrolls: 3,
            step_timeout_ms: opts.step_timeout_ms,
            post_settle_quiescence_ms: opts.post_settle_quiescence_ms,
            settle_similarity_threshold: opts.settle_similarity_threshold,
            confirmed: opts.confirmed,
            prefer_graph_route: false,
            secure_handoff_timeout_secs: opts.secure_handoff_timeout_secs,
        };
        let outcome = self
            .perform_explore_and_click(target, serial, executor, explore_opts, true)
            .await?;
        Ok(match outcome {
            ExploreOutcome::ArrivedViaGraph {
                report,
                already_at_target,
                active_package,
            } => {
                if already_at_target && report.steps.is_empty() {
                    ShadowNavigationOutcome::AlreadyAtTarget {
                        state_hash: report.final_hash,
                        active_package,
                    }
                } else {
                    ShadowNavigationOutcome::Arrived(report)
                }
            }
            ExploreOutcome::RequiresConfirmation(req) => {
                ShadowNavigationOutcome::RequiresConfirmation(req)
            }
            ExploreOutcome::SecureScreenTimeout {
                reason,
                active_package,
                timeout_seconds,
            } => ShadowNavigationOutcome::SecureScreenTimeout {
                reason,
                active_package,
                timeout_seconds,
            },
            other => ShadowNavigationOutcome::Explored(other),
        })
    }

    /// Dynamic live-screen discovery + tap with optional A* graph/prefix routing, bounded scrolling,
    /// HITL hazard checks, heads-up banner dismissal, and DAG edge recording.
    pub async fn explore_and_click(
        &self,
        target: &str,
        serial: &str,
        executor: Arc<dyn MacroActionExecutor>,
        opts: ExploreOptions,
    ) -> Result<ExploreOutcome, PathDeviationError> {
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

        let allow_prefix = opts.prefer_graph_route;
        self.perform_explore_and_click(target, serial, executor, opts, allow_prefix)
            .await
    }

    async fn perform_explore_and_click(
        &self,
        target: &str,
        serial: &str,
        executor: Arc<dyn MacroActionExecutor>,
        opts: ExploreOptions,
        allow_prefix_route: bool,
    ) -> Result<ExploreOutcome, PathDeviationError> {
        let total_start = Instant::now();
        let trimmed_target = target.trim();

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
            return Err(PathDeviationError::ShadowStreamUnavailable);
        }

        // Check FLAG_SECURE / biometric lock before starting
        if let Some(timeout_outcome) = self
            .await_explore_secure_screen(opts.secure_handoff_timeout_secs)
            .await
        {
            return Ok(timeout_outcome);
        }

        let mut active_query = trimmed_target.to_string();

        // Step 1: When prefer_graph_route == true, try full A* first, then nearest known prefix vertex.
        if opts.prefer_graph_route && !active_query.is_empty() {
            let graph_opts = RouteExecuteOptions {
                target_hint: Some(active_query.clone()),
                target_hash: None,
                max_steps: 12,
                step_timeout_ms: opts.step_timeout_ms,
                post_settle_quiescence_ms: opts.post_settle_quiescence_ms,
                settle_similarity_threshold: opts.settle_similarity_threshold,
                confirmed: opts.confirmed,
                enable_auto_recovery: true,
                auto_explore: false,
                secure_handoff_timeout_secs: opts.secure_handoff_timeout_secs,
            };
            if let Ok(nav_outcome) =
                Box::pin(self.perform_route_and_execute(serial, Arc::clone(&executor), graph_opts))
                    .await
            {
                match nav_outcome {
                    ShadowNavigationOutcome::Arrived(report) => {
                        let pkg = self
                            .dom_state
                            .current_index()
                            .await
                            .map(|i| i.active_package.clone())
                            .unwrap_or_default();
                        return Ok(ExploreOutcome::ArrivedViaGraph {
                            report,
                            already_at_target: false,
                            active_package: pkg,
                        });
                    }
                    ShadowNavigationOutcome::AlreadyAtTarget {
                        state_hash,
                        active_package,
                    } => {
                        // Only treat as already-at-target if there is NO clickable live element
                        // matching the query on the current screen (e.g. cold-start vertex whose
                        // sampleInteractiveLabels included the button we actually want to click).
                        let has_clickable_match = if let Some(idx) =
                            self.dom_state.current_index().await
                        {
                            let scored = Self::score_live_candidates(
                                &idx,
                                &active_query,
                                &self.hasher.ignored_packages,
                            );
                            scored
                                .first()
                                .is_some_and(|c| c.score >= 500 && c.clickable)
                        } else {
                            false
                        };
                        if !has_clickable_match {
                            return Ok(ExploreOutcome::ArrivedViaGraph {
                                report: MacroExecutionReport {
                                    start_hash: state_hash,
                                    final_hash: state_hash,
                                    steps: Vec::new(),
                                    total_elapsed_ms: total_start.elapsed().as_millis() as u64,
                                    recovered_steps: Vec::new(),
                                },
                                already_at_target: true,
                                active_package,
                            });
                        }
                    }
                    ShadowNavigationOutcome::RequiresConfirmation(req) => {
                        return Ok(ExploreOutcome::RequiresConfirmation(req));
                    }
                    ShadowNavigationOutcome::SecureScreenTimeout {
                        reason,
                        active_package,
                        timeout_seconds,
                    } => {
                        return Ok(ExploreOutcome::SecureScreenTimeout {
                            reason,
                            active_package,
                            timeout_seconds,
                        });
                    }
                    ShadowNavigationOutcome::NoRoute { .. }
                    | ShadowNavigationOutcome::TargetNotFound { .. }
                    | ShadowNavigationOutcome::Explored(_)
                    | ShadowNavigationOutcome::StreamUnavailable => {}
                }
            }
        }

        // Prefix routing: if target is a breadcrumb (e.g. "Display > Night Light") or multi-word
        // phrase whose prefix maps to a reachable known graph vertex, ride A* to that prefix first.
        if allow_prefix_route && !active_query.is_empty() {
            let prefix_pairs = Self::extract_prefix_and_leaf_candidates(&active_query);
            let has_explicit_breadcrumb = prefix_pairs.first().is_some_and(|(_, _, explicit)| *explicit);
            let already_has_confident_live_match = if !has_explicit_breadcrumb {
                if let Some(cur_idx) = self.dom_state.current_index().await {
                    Self::score_live_candidates(&cur_idx, &active_query, &self.hasher.ignored_packages)
                        .first()
                        .is_some_and(|c| c.score >= 500)
                } else {
                    false
                }
            } else {
                false
            };

            if !already_has_confident_live_match {
                for (prefix_query, leaf_query, is_explicit_breadcrumb) in prefix_pairs {
                    let snap = self.graph_engine.snapshot().await;
                    let ranked = self
                        .graph_engine
                        .resolve_target_vertices(&prefix_query, 4)
                        .await;
                    let Some(best_prefix) = ranked.first() else {
                        continue;
                    };
                    if best_prefix.score < 500 {
                        continue;
                    }
                    let Some(cur_idx) = self.dom_state.current_index().await else {
                        break;
                    };
                    let cur_fp = self.hasher.fingerprint(&cur_idx);
                    let pf = AStarPathfinder::new(AStarCostConfiguration::default());
                    let mut goals = HashSet::new();
                    goals.insert(best_prefix.vertex.layout_hash);
                    let route_opt = pf
                        .find_shortest_path_to_any(cur_fp.layout_hash, &goals, &snap)
                        .or_else(|| {
                            Self::semantic_aliases(
                                &cur_fp,
                                cur_fp.layout_hash,
                                &snap.vertices,
                                opts.settle_similarity_threshold,
                            )
                            .into_iter()
                            .find_map(|(ah, _)| pf.find_shortest_path_to_any(ah, &goals, &snap))
                        });

                    let Some(route) = route_opt else {
                        continue;
                    };

                    if route.edges.is_empty() {
                        // Only treat the current vertex as the resolved prefix screen when the caller
                        // gave an explicit breadcrumb (e.g. "Display > Night Light") AND we are genuinely
                        // inside that screen (not merely seeing a clickable button named `prefix_query`
                        // on a parent menu like Settings root).
                        if is_explicit_breadcrumb
                            && !self.should_auto_explore_instead_of_already_at_target(
                                &prefix_query,
                                cur_fp.layout_hash,
                                &cur_idx,
                                &snap,
                            )
                        {
                            active_query = leaf_query;
                            break;
                        }
                        continue;
                    }

                    let prefix_opts = RouteExecuteOptions {
                        target_hint: None,
                        target_hash: Some(best_prefix.vertex.layout_hash),
                        max_steps: 12,
                        step_timeout_ms: opts.step_timeout_ms,
                        post_settle_quiescence_ms: opts.post_settle_quiescence_ms,
                        settle_similarity_threshold: opts.settle_similarity_threshold,
                        confirmed: opts.confirmed,
                        enable_auto_recovery: true,
                        auto_explore: false,
                        secure_handoff_timeout_secs: opts.secure_handoff_timeout_secs,
                    };
                    match Box::pin(self.perform_route_and_execute(
                        serial,
                        Arc::clone(&executor),
                        prefix_opts,
                    ))
                    .await
                    {
                        Ok(ShadowNavigationOutcome::Arrived(_))
                        | Ok(ShadowNavigationOutcome::AlreadyAtTarget { .. }) => {
                            active_query = leaf_query;
                            break;
                        }
                        Ok(ShadowNavigationOutcome::RequiresConfirmation(req)) => {
                            return Ok(ExploreOutcome::RequiresConfirmation(req));
                        }
                        Ok(ShadowNavigationOutcome::SecureScreenTimeout {
                            reason,
                            active_package,
                            timeout_seconds,
                        }) => {
                            return Ok(ExploreOutcome::SecureScreenTimeout {
                                reason,
                                active_package,
                                timeout_seconds,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }

        // Step 2–4: Live ShadowTreeIndex scoring + bounded scroll discovery loop.
        let mut scrolls_performed = 0usize;
        let mut scrolling_down = true;
        let mut reversed_direction = false;

        loop {
            if let Some(timeout_outcome) = self
                .await_explore_secure_screen(opts.secure_handoff_timeout_secs)
                .await
            {
                return Ok(timeout_outcome);
            }

            let Some(current_index) = self.dom_state.current_index().await else {
                return Err(PathDeviationError::ShadowStreamUnavailable);
            };

            let scored = if active_query.is_empty() {
                Vec::new()
            } else {
                Self::score_live_candidates(
                    &current_index,
                    &active_query,
                    &self.hasher.ignored_packages,
                )
            };

            // Step 3: Found a strong match (score >= 500) -> validate hazard & banner, tap, settle, record DAG edge.
            if let Some(best_candidate) = scored.first().filter(|c| c.score >= 500).cloned() {
                return self
                    .execute_explore_candidate_tap(
                        best_candidate,
                        &current_index,
                        &active_query,
                        scrolls_performed,
                        total_start,
                        serial,
                        &executor,
                        &opts,
                    )
                    .await;
            }

            // Step 4: Bounded scrolling when not found yet.
            if scrolls_performed >= opts.max_scrolls {
                break;
            }

            let Some(scroll_rect) = MacroExecutionEngine::largest_scrollable_rect(
                &current_index,
                &self.hasher.ignored_packages,
            ) else {
                break;
            };

            let before_fp = self.hasher.fingerprint(&current_index);
            let before_sig = MacroExecutionEngine::scroll_viewport_signature(
                &current_index,
                scroll_rect,
                &self.hasher.ignored_packages,
            );
            let before_hash = self
                .seed_vertex_if_absent(&current_index, before_fp.layout_hash)
                .await;
            let scroll_start = Instant::now();

            let zero_fling = executor.supports_zero_fling_swipes(serial).await;
            let cx = ((scroll_rect.min_x + scroll_rect.max_x) / 2).max(1);
            let span_y = scroll_rect.max_y - scroll_rect.min_y;
            let (high_pct, low_pct, swipe_dur_ms) = if zero_fling {
                (80, 18, 145)
            } else {
                (72, 28, 500)
            };
            let y_high = scroll_rect.min_y + (span_y * high_pct) / 100;
            let y_low = scroll_rect.min_y + (span_y * low_pct) / 100;
            let (start_y, end_y, dir_label) = if scrolling_down {
                (y_high, y_low, "down")
            } else {
                (y_low, y_high, "up")
            };

            let swipe_action = AttributedUIAction::swipe(
                format!("explore_scroll_{dir_label}"),
                cx,
                start_y,
                cx,
                end_y,
                Some(swipe_dur_ms),
            );

            self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
                kind: "explore_scroll".to_string(),
                detail: format!(
                    "Scrolling {dir_label} ({}/{}) searching for '{active_query}'",
                    scrolls_performed + 1,
                    opts.max_scrolls
                ),
            });

            // Race-free subscribe + record scroll pending action into SpatialGraphEngine
            let mut update_rx = self.dom_state.subscribe_updates();
            self.graph_engine
                .record_pending_action(swipe_action.clone())
                .await;
            executor.execute(&swipe_action, serial).await?;

            let wait_ms = opts.step_timeout_ms.clamp(150, 1200);
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
                                let next_fp = self.hasher.fingerprint(&upd.index);
                                if next_fp.tokens.is_empty()
                                    || Self::is_transient_ignored_system_frame(
                                        &upd.index,
                                        &next_fp.active_package,
                                        &before_fp.active_package,
                                        &self.hasher.ignored_packages,
                                    )
                                {
                                    continue;
                                }
                                let next_sig = MacroExecutionEngine::scroll_viewport_signature(
                                    &upd.index,
                                    scroll_rect,
                                    &self.hasher.ignored_packages,
                                );
                                if next_fp.layout_hash != before_fp.layout_hash || next_sig != before_sig {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }

            if opts.post_settle_quiescence_ms > 0 {
                if zero_fling {
                    // Drain any in-flight mid-swipe and post-ACTION_UP accessibility frames
                    // until the Shadow DOM stream stays quiet (debounce + UiAutomation collect
                    // takes ~70-95ms after ACTION_UP), ensuring we never record a mid-swipe hash.
                    let quiet_ms = opts.post_settle_quiescence_ms.max(115);
                    let max_drain_ms = quiet_ms.max(380);
                    let hard_deadline = tokio::time::sleep(Duration::from_millis(max_drain_ms));
                    let quiet_deadline = tokio::time::sleep(Duration::from_millis(quiet_ms));
                    tokio::pin!(hard_deadline);
                    tokio::pin!(quiet_deadline);
                    loop {
                        tokio::select! {
                            _ = &mut hard_deadline => break,
                            _ = &mut quiet_deadline => break,
                            res = update_rx.recv() => {
                                match res {
                                    Ok(upd) if !upd.is_secure_screen_active => {
                                        quiet_deadline
                                            .as_mut()
                                            .reset(tokio::time::Instant::now() + Duration::from_millis(quiet_ms));
                                    }
                                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                    Err(broadcast::error::RecvError::Closed) => break,
                                }
                            }
                        }
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(
                        opts.post_settle_quiescence_ms.max(400),
                    ))
                    .await;
                }
            }

            // Record scroll transition edge into SpatialGraphEngine
            if let Some(after_idx) = self.dom_state.current_index().await {
                let after_fp = self.hasher.fingerprint(&after_idx);
                // If a swipe accidentally pulled down a system overlay (e.g. notification shade),
                // abort scrolling without poisoning the DAG with a systemui transition edge.
                if after_fp.active_package != before_fp.active_package
                    && self.hasher.ignored_packages.contains(&after_fp.active_package)
                {
                    scrolls_performed += 1;
                    break;
                }

                let after_update = ShadowDOMUpdate {
                    index: Arc::clone(&after_idx),
                    is_full_snapshot: false,
                    inserted_count: 0,
                    updated_count: after_idx.ordered_nodes.len(),
                    removed_count: 0,
                    event_type: None,
                    event_package: Some(after_idx.active_package.clone()),
                    is_secure_screen_active: false,
                };
                let _ = self.graph_engine.process(&after_update).await;

                // If mid-scroll deceleration frames consumed `pending_action` before the list
                // settled, bridge `(before_hash -> settled_after_hash)` directly with `swipe_action`
                // so A* has a continuous executable edge across scrolled screens (no implicit gap).
                let resolved_after_hash = if after_fp.layout_hash == before_fp.layout_hash {
                    before_hash
                } else {
                    self.graph_engine
                        .current_state_hash()
                        .await
                        .unwrap_or(after_fp.layout_hash)
                };
                if resolved_after_hash != before_hash {
                    let neighbors = self.graph_engine.neighbors(before_hash).await;
                    let has_direct_exec = neighbors
                        .iter()
                        .any(|e| e.to_hash == resolved_after_hash && e.action.is_some());
                    if !has_direct_exec {
                        let _ = self
                            .graph_engine
                            .record_direct_edge(
                                before_hash,
                                resolved_after_hash,
                                swipe_action.clone(),
                                scroll_start.elapsed().as_millis() as f64,
                            )
                            .await;
                    }
                }

                let after_sig = MacroExecutionEngine::scroll_viewport_signature(
                    &after_idx,
                    scroll_rect,
                    &self.hasher.ignored_packages,
                );
                scrolls_performed += 1;

                // Compare post-quiescence viewport (ignoring transient overscroll spring frames)
                if after_fp.layout_hash == before_fp.layout_hash && after_sig == before_sig {
                    if scrolling_down && !reversed_direction {
                        scrolling_down = false;
                        reversed_direction = true;
                    } else {
                        break;
                    }
                }
            } else {
                scrolls_performed += 1;
                break;
            }
        }

        // Step 5: Not found -> return top-20 PII-scrubbed interactive candidates (`isError: false`).
        let final_index = self
            .dom_state
            .current_index()
            .await
            .ok_or(PathDeviationError::ShadowStreamUnavailable)?;
        let final_fp = self.hasher.fingerprint(&final_index);
        let candidates = Self::collect_top_interactive_candidates(
            &final_index,
            &active_query,
            &self.hasher.ignored_packages,
            20,
        );

        Ok(ExploreOutcome::NeedsSelection {
            query: trimmed_target.to_string(),
            current_hash: final_fp.layout_hash,
            active_package: final_fp.active_package,
            scrolls_performed,
            candidates,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_explore_candidate_tap(
        &self,
        mut candidate: ExploreCandidate,
        initial_index: &Arc<ShadowTreeIndex>,
        active_query: &str,
        scrolls_performed: usize,
        total_start: Instant,
        serial: &str,
        executor: &Arc<dyn MacroActionExecutor>,
        opts: &ExploreOptions,
    ) -> Result<ExploreOutcome, PathDeviationError> {
        let mut live_index = Arc::clone(initial_index);
        let mut raw_node = live_index.nodes_by_id.get(&candidate.node_id).cloned();

        // Build attributed tap action
        let build_action = |cand: &ExploreCandidate, node_opt: Option<&ShadowUINode>| {
            let desc = if !cand.label.is_empty() {
                cand.label.clone()
            } else if !cand.resource_id.is_empty() {
                cand.resource_id.clone()
            } else {
                format!("({},{})", cand.center_x, cand.center_y)
            };
            let mut act =
                AttributedUIAction::tap(desc, Some(cand.center_x), Some(cand.center_y));
            if let Some(n) = node_opt {
                act.selector_resource_id = (!n.resource_id.is_empty()).then(|| n.resource_id.clone());
                act.selector_text = (!n.text.is_empty()).then(|| n.text.clone());
                act.selector_content_desc =
                    (!n.content_desc.is_empty()).then(|| n.content_desc.clone());
            } else if !cand.resource_id.is_empty() {
                act.selector_resource_id = Some(cand.resource_id.clone());
            }
            act
        };

        let mut action = build_action(&candidate, raw_node.as_ref());

        // Hazardous / irreversible transaction HITL check
        let raw_label_ref = raw_node
            .as_ref()
            .map(|n| {
                if !n.text.is_empty() {
                    n.text.as_str()
                } else {
                    n.content_desc.as_str()
                }
            })
            .unwrap_or(candidate.label.as_str());
        let hazard = HazardousActionClassifier::assess(
            Some(&action),
            &action.signature(),
            Some(raw_label_ref),
            Some(candidate.resource_id.as_str()),
        );
        if hazard.is_hazardous && !opts.confirmed {
            let current_hash = self.hasher.fingerprint(&live_index).layout_hash;
            let req = HazardousConfirmationRequest {
                step_index: 0,
                total_steps: 1,
                action_signature: action.signature(),
                target_description: action.target_description.clone(),
                hazard_category: hazard.category.unwrap_or(HazardCategory::Destructive),
                hazard_reason: hazard.reason(),
                current_hash,
                target_hash: current_hash,
                steps_completed: 0,
                tap_x: action.x,
                tap_y: action.y,
            };
            self.emit_macro_event(MacroExecutionEvent::AwaitingConfirmation {
                step_index: 0,
                total_steps: 1,
                action,
                reason: req.hazard_reason.clone(),
                request: req.clone(),
            });
            return Ok(ExploreOutcome::RequiresConfirmation(req));
        }

        // Heads-up notification banner dismissal if occluding tap point
        for attempt in 0..2usize {
            let Some(banner_rect) = MacroExecutionEngine::occluding_system_banner_rect(
                &live_index,
                candidate.center_x,
                candidate.center_y,
            ) else {
                break;
            };
            let swipe_x = ((banner_rect.min_x + banner_rect.max_x) / 2).max(1);
            let start_y = (banner_rect.max_y - 20).max(20);
            let end_y = (banner_rect.min_y - 120).max(10);
            let dismiss_swipe = AttributedUIAction::swipe(
                "Dismiss occluding notification",
                swipe_x,
                start_y,
                swipe_x,
                end_y,
                Some(160),
            );
            self.emit_macro_event(MacroExecutionEvent::RecoveryAction {
                kind: "dismiss_notification_overlay".to_string(),
                detail: format!(
                    "Swiped up occluding system notification ({}/2)",
                    attempt + 1
                ),
            });
            let mut banner_rx = self.dom_state.subscribe_updates();
            executor.execute(&dismiss_swipe, serial).await?;

            let deadline = tokio::time::sleep(Duration::from_millis(450));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    res = banner_rx.recv() => {
                        if let Ok(upd) = res {
                            if MacroExecutionEngine::occluding_system_banner_rect(
                                &upd.index,
                                candidate.center_x,
                                candidate.center_y,
                            ).is_none() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
            }

            if let Some(refreshed_idx) = self.dom_state.current_index().await {
                live_index = refreshed_idx;
                if let Some(refreshed_cand) = Self::score_live_candidates(
                    &live_index,
                    active_query,
                    &self.hasher.ignored_packages,
                )
                .into_iter()
                .next()
                .filter(|c| c.score >= 500)
                {
                    candidate = refreshed_cand;
                    raw_node = live_index.nodes_by_id.get(&candidate.node_id).cloned();
                    action = build_action(&candidate, raw_node.as_ref());

                    if !opts.confirmed {
                        let post_raw_label = raw_node
                            .as_ref()
                            .map(|n| {
                                if !n.text.is_empty() {
                                    n.text.as_str()
                                } else {
                                    n.content_desc.as_str()
                                }
                            })
                            .unwrap_or(candidate.label.as_str());
                        let post_hazard = HazardousActionClassifier::assess(
                            Some(&action),
                            &action.signature(),
                            Some(post_raw_label),
                            Some(candidate.resource_id.as_str()),
                        );
                        if post_hazard.is_hazardous {
                            let current_hash = self.hasher.fingerprint(&live_index).layout_hash;
                            let req = HazardousConfirmationRequest {
                                step_index: 0,
                                total_steps: 1,
                                action_signature: action.signature(),
                                target_description: action.target_description.clone(),
                                hazard_category: post_hazard
                                    .category
                                    .unwrap_or(HazardCategory::Destructive),
                                hazard_reason: post_hazard.reason(),
                                current_hash,
                                target_hash: current_hash,
                                steps_completed: 0,
                                tap_x: action.x,
                                tap_y: action.y,
                            };
                            self.emit_macro_event(MacroExecutionEvent::AwaitingConfirmation {
                                step_index: 0,
                                total_steps: 1,
                                action,
                                reason: req.hazard_reason.clone(),
                                request: req.clone(),
                            });
                            return Ok(ExploreOutcome::RequiresConfirmation(req));
                        }
                    }
                }
            }
        }

        let before_fp = self.hasher.fingerprint(&live_index);
        let before_hash = self
            .seed_vertex_if_absent(&live_index, before_fp.layout_hash)
            .await;
        let edges_before = self.graph_engine.edge_count().await;

        // Race-free subscribe + record pending action BEFORE tap
        let mut update_rx = self.dom_state.subscribe_updates();
        let mut secure_rx = self.dom_state.subscribe_secure_screen();
        self.graph_engine.record_pending_action(action.clone()).await;

        self.emit_macro_event(MacroExecutionEvent::WillExecuteAction {
            step_index: 0,
            total_steps: 1,
            action: action.clone(),
        });

        executor.execute(&action, serial).await?;

        let wait_ms = opts.step_timeout_ms.max(100);
        let deadline = tokio::time::sleep(Duration::from_millis(wait_ms));
        tokio::pin!(deadline);
        let mut settled_update: Option<ShadowDOMUpdate> = None;
        let mut interrupted_by_secure = false;

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                upd = update_rx.recv() => {
                    match upd {
                        Ok(u) => {
                            if u.is_secure_screen_active {
                                continue;
                            }
                            let fp = self.hasher.fingerprint(&u.index);
                            if fp.tokens.is_empty()
                                || Self::is_transient_ignored_system_frame(
                                    &u.index,
                                    &fp.active_package,
                                    &before_fp.active_package,
                                    &self.hasher.ignored_packages,
                                )
                            {
                                continue;
                            }
                            if fp.layout_hash != before_fp.layout_hash
                                && fp.layout_hash != before_hash
                            {
                                settled_update = Some(u);
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                sec = secure_rx.recv() => {
                    if let Ok(payload) = sec {
                        if payload.active {
                            interrupted_by_secure = true;
                            break;
                        }
                    }
                }
            }
        }

        if interrupted_by_secure {
            if let Some(timeout_outcome) = self
                .await_explore_secure_screen(opts.secure_handoff_timeout_secs)
                .await
            {
                return Ok(timeout_outcome);
            }
        }

        if opts.post_settle_quiescence_ms > 0 && settled_update.is_some() {
            // Debounce async multi-frame inflation (e.g. Android Settings header inflating 200-400ms
            // after window creation) so the final settled vertex is captured cleanly.
            let quiet_ms = opts.post_settle_quiescence_ms.max(220);
            let max_cap = tokio::time::sleep(Duration::from_millis(quiet_ms.saturating_mul(3).max(750)));
            tokio::pin!(max_cap);
            let quiet_timer = tokio::time::sleep(Duration::from_millis(quiet_ms));
            tokio::pin!(quiet_timer);
            loop {
                tokio::select! {
                    _ = &mut max_cap => break,
                    _ = &mut quiet_timer => break,
                    upd = update_rx.recv() => {
                        match upd {
                            Ok(u) => {
                                if u.is_secure_screen_active {
                                    continue;
                                }
                                let fp = self.hasher.fingerprint(&u.index);
                                if fp.tokens.is_empty()
                                    || Self::is_transient_ignored_system_frame(
                                        &u.index,
                                        &fp.active_package,
                                        &before_fp.active_package,
                                        &self.hasher.ignored_packages,
                                    )
                                {
                                    continue;
                                }
                                settled_update = Some(u);
                                quiet_timer.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(quiet_ms));
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }

        let live_after = self.dom_state.current_index().await;
        let final_index = match (live_after, settled_update.as_ref()) {
            (Some(idx), Some(su))
                if Self::is_transient_ignored_system_frame(
                    &idx,
                    &idx.active_package,
                    &before_fp.active_package,
                    &self.hasher.ignored_packages,
                ) =>
            {
                Arc::clone(&su.index)
            }
            (Some(idx), _) => idx,
            (None, Some(su)) => Arc::clone(&su.index),
            (None, None) => live_index,
        };
        let final_fp = self.hasher.fingerprint(&final_index);

        let commit_update = ShadowDOMUpdate {
            index: Arc::clone(&final_index),
            is_full_snapshot: false,
            inserted_count: 0,
            updated_count: final_index.ordered_nodes.len(),
            removed_count: 0,
            event_type: None,
            event_package: Some(final_index.active_package.clone()),
            is_secure_screen_active: false,
        };
        let mut processed_edge = self.graph_engine.process(&commit_update).await.is_some();

        // If intermediate skeleton frames consumed `pending_action` before the final settled screen
        // finished inflating, bridge `(before_hash -> settled_final_hash)` directly with `action`
        // so A* has a clean executable edge without an implicit gap.
        let resolved_final_hash = if final_fp.layout_hash == before_fp.layout_hash {
            before_hash
        } else {
            self.graph_engine
                .current_state_hash()
                .await
                .unwrap_or(final_fp.layout_hash)
        };
        if resolved_final_hash != before_hash {
            let neighbors = self.graph_engine.neighbors(before_hash).await;
            let has_direct_exec = neighbors
                .iter()
                .any(|e| e.to_hash == resolved_final_hash && e.action.is_some());
            if !has_direct_exec
                && self
                    .graph_engine
                    .record_direct_edge(
                        before_hash,
                        resolved_final_hash,
                        action.clone(),
                        total_start.elapsed().as_millis() as f64,
                    )
                    .await
                    .is_some()
            {
                processed_edge = true;
            }
        }

        let edge_recorded = if resolved_final_hash == before_hash {
            false
        } else if processed_edge || self.graph_engine.edge_count().await > edges_before {
            true
        } else {
            let neighbors = self.graph_engine.neighbors(before_hash).await;
            neighbors.iter().any(|e| e.to_hash == resolved_final_hash)
        };
        let screen_changed = resolved_final_hash != before_hash;

        Ok(ExploreOutcome::ExploredAndClicked {
            matched_candidate: candidate,
            scrolls_performed,
            start_hash: before_hash,
            final_hash: resolved_final_hash,
            active_package: final_fp.active_package,
            screen_changed,
            edge_recorded,
            total_elapsed_ms: total_start.elapsed().as_millis() as u64,
        })
    }

    async fn seed_vertex_if_absent(&self, index: &Arc<ShadowTreeIndex>, raw_hash: u64) -> u64 {
        let mut effective_hash = raw_hash;
        if self.graph_engine.vertex(raw_hash).await.is_none()
            || self.graph_engine.current_state_hash().await.is_none()
        {
            let _ = self
                .graph_engine
                .process(&ShadowDOMUpdate {
                    index: Arc::clone(index),
                    is_full_snapshot: true,
                    inserted_count: index.ordered_nodes.len(),
                    updated_count: 0,
                    removed_count: 0,
                    event_type: None,
                    event_package: Some(index.active_package.clone()),
                    is_secure_screen_active: false,
                })
                .await;
            if let Some(cur) = self.graph_engine.current_state_hash().await {
                effective_hash = cur;
            }
        }
        effective_hash
    }

    async fn await_explore_secure_screen(&self, timeout_secs: f64) -> Option<ExploreOutcome> {
        let (is_secure, sec_reason, sec_pkg) = self.dom_state.secure_screen_info().await;
        if !is_secure {
            return None;
        }
        let reason = sec_reason.unwrap_or_else(|| "biometric_or_pin".to_string());
        self.emit_macro_event(MacroExecutionEvent::AwaitingBiometrics {
            reason: reason.clone(),
            active_package: sec_pkg.clone(),
        });
        let cleared = self
            .dom_state
            .wait_until_secure_screen_cleared(timeout_secs)
            .await;
        if !cleared {
            let timeout_sec = timeout_secs.round() as u64;
            self.emit_macro_event(MacroExecutionEvent::Failed {
                message: format!("Secure screen remained locked past {timeout_sec}s"),
            });
            return Some(ExploreOutcome::SecureScreenTimeout {
                reason,
                active_package: sec_pkg,
                timeout_seconds: timeout_sec,
            });
        }
        self.emit_macro_event(MacroExecutionEvent::BiometricsCleared);
        None
    }

    /// Scores visible non-system UI nodes against `query`:
    /// exact = 1000, prefix = 700, resourceId suffix = 600, contains = 500,
    /// token overlap = 250–450, +100 for clickable or clickable ancestor.
    pub fn score_live_candidates(
        index: &ShadowTreeIndex,
        query: &str,
        ignored_packages: &HashSet<String>,
    ) -> Vec<ExploreCandidate> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let q_lower = trimmed.to_lowercase();
        let q_tokens = Self::tokenize_words(&q_lower);
        let skip_ignored = !ignored_packages.contains(&index.active_package);

        let mut scored: Vec<(ExploreCandidate, bool, i64, i32)> = Vec::new();

        for node in &index.ordered_nodes {
            if !node.visible || (node.center_x <= 0 && node.center_y <= 0) {
                continue;
            }
            if skip_ignored && Self::is_ignored_node(node, ignored_packages) {
                continue;
            }
            if MacroExecutionEngine::is_node_clipped_by_scrollable_ancestor(
                index,
                node,
                ignored_packages,
            ) {
                continue;
            }
            let area = ShadowTreeIndex::parse_bounds_rect(&node.bounds)
                .map(|r| r.area())
                .unwrap_or(i64::MAX);

            let base_score = Self::node_match_base_score(node, &q_lower, &q_tokens);
            if base_score <= 0 {
                continue;
            }
            let effective_clickable =
                node.clickable || Self::has_clickable_ancestor(index, node);
            let total_score = base_score + if effective_clickable { 100 } else { 0 };

            let raw_label = if !node.text.is_empty() {
                node.text.as_str()
            } else if !node.content_desc.is_empty() {
                node.content_desc.as_str()
            } else {
                ShadowTreeIndex::extract_short_resource_id(&node.resource_id)
            };
            let safe_label = ScreenLayoutHasher::redact_pii(raw_label);

            scored.push((
                ExploreCandidate {
                    node_id: node.node_id,
                    label: safe_label,
                    resource_id: node.resource_id.clone(),
                    class_name: node.class_name.clone(),
                    center_x: node.center_x,
                    center_y: node.center_y,
                    clickable: effective_clickable,
                    scrollable: node.scrollable,
                    score: total_score,
                },
                node.enabled,
                area,
                node.index,
            ));
        }

        scored.sort_by(|lhs, rhs| {
            rhs.0
                .score
                .cmp(&lhs.0.score)
                .then_with(|| rhs.1.cmp(&lhs.1))
                .then_with(|| lhs.2.cmp(&rhs.2))
                .then_with(|| lhs.3.cmp(&rhs.3))
        });

        scored.into_iter().map(|(c, _, _, _)| c).collect()
    }

    /// Returns up to `limit` PII-scrubbed interactive candidates on the live screen for LLM selection.
    pub fn collect_top_interactive_candidates(
        index: &ShadowTreeIndex,
        query: &str,
        ignored_packages: &HashSet<String>,
        limit: usize,
    ) -> Vec<ExploreCandidate> {
        let q_lower = query.trim().to_lowercase();
        let q_tokens = Self::tokenize_words(&q_lower);
        let skip_ignored = !ignored_packages.contains(&index.active_package);

        let mut raw_list: Vec<(ExploreCandidate, i32)> = Vec::new();
        let mut seen_keys: HashSet<(String, i32, i32)> = HashSet::new();

        for node in &index.ordered_nodes {
            if !node.visible || (node.center_x <= 0 && node.center_y <= 0) {
                continue;
            }
            if skip_ignored && Self::is_ignored_node(node, ignored_packages) {
                continue;
            }
            if MacroExecutionEngine::is_node_clipped_by_scrollable_ancestor(
                index,
                node,
                ignored_packages,
            ) {
                continue;
            }
            let effective_clickable =
                node.clickable || Self::has_clickable_ancestor(index, node);
            let raw_label = if !node.text.is_empty() {
                node.text.as_str()
            } else if !node.content_desc.is_empty() {
                node.content_desc.as_str()
            } else {
                ShadowTreeIndex::extract_short_resource_id(&node.resource_id)
            };
            let is_interactive = effective_clickable || node.scrollable || node.editable;
            if !is_interactive && raw_label.is_empty() {
                continue;
            }

            let safe_label: String = ScreenLayoutHasher::redact_pii(raw_label)
                .chars()
                .take(64)
                .collect();
            if safe_label.is_empty() && node.resource_id.is_empty() {
                continue;
            }

            let dedup_key = (safe_label.clone(), node.center_x, node.center_y);
            if !seen_keys.insert(dedup_key) {
                continue;
            }

            let base_score = if q_lower.is_empty() {
                0
            } else {
                Self::node_match_base_score(node, &q_lower, &q_tokens)
            };
            let score = if base_score > 0 {
                base_score + if effective_clickable { 100 } else { 0 }
            } else {
                0
            };

            raw_list.push((
                ExploreCandidate {
                    node_id: node.node_id,
                    label: safe_label,
                    resource_id: node.resource_id.clone(),
                    class_name: node.class_name.clone(),
                    center_x: node.center_x,
                    center_y: node.center_y,
                    clickable: effective_clickable,
                    scrollable: node.scrollable,
                    score,
                },
                node.index,
            ));
        }

        raw_list.sort_by(|lhs, rhs| {
            rhs.0
                .score
                .cmp(&lhs.0.score)
                .then_with(|| rhs.0.clickable.cmp(&lhs.0.clickable))
                .then_with(|| rhs.0.label.is_empty().cmp(&lhs.0.label.is_empty()).reverse())
                .then_with(|| lhs.1.cmp(&rhs.1))
        });

        raw_list
            .into_iter()
            .take(limit)
            .map(|(c, _)| c)
            .collect()
    }

    fn node_match_base_score(node: &ShadowUINode, q_lower: &str, q_tokens: &[String]) -> i32 {
        let mut best = 0;

        for candidate_raw in [node.text.as_str(), node.content_desc.as_str()] {
            let trimmed = candidate_raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            if lower == q_lower {
                best = best.max(1000);
            } else if lower.starts_with(q_lower) {
                best = best.max(700);
            } else if lower.contains(q_lower) {
                best = best.max(500);
            } else if !q_tokens.is_empty() {
                let n_tokens = Self::tokenize_words(&lower);
                best = best.max(Self::token_overlap_score(q_tokens, &n_tokens));
            }
        }

        if !node.resource_id.is_empty() {
            let rid_lower = node.resource_id.to_lowercase();
            let short_rid = ShadowTreeIndex::extract_short_resource_id(&rid_lower);
            if rid_lower == q_lower
                || short_rid == q_lower
                || (q_lower.len() >= 2 && short_rid.ends_with(q_lower))
            {
                best = best.max(600);
            } else if !q_tokens.is_empty() {
                let rid_tokens = Self::tokenize_words(short_rid);
                best = best.max(Self::token_overlap_score(q_tokens, &rid_tokens));
            }
        }

        best
    }

    fn token_overlap_score(q_tokens: &[String], n_tokens: &[String]) -> i32 {
        if q_tokens.is_empty() || n_tokens.is_empty() {
            return 0;
        }
        let n_set: HashSet<&str> = n_tokens.iter().map(String::as_str).collect();
        let q_set: HashSet<&str> = q_tokens.iter().map(String::as_str).collect();
        let overlap = q_set.iter().filter(|t| n_set.contains(**t)).count();
        if overlap == 0 {
            return 0;
        }
        if q_set.len() >= 2 && overlap == q_set.len() {
            return 450;
        }
        let union = q_set.len() + n_set.len() - overlap;
        if union == 0 {
            return 0;
        }
        let jaccard = overlap as f64 / union as f64;
        (250.0 + 140.0 * jaccard).round().clamp(250.0, 390.0) as i32
    }

    fn tokenize_words(s: &str) -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    fn has_clickable_ancestor(index: &ShadowTreeIndex, node: &ShadowUINode) -> bool {
        let mut cursor = node.parent_id;
        let mut hops = 0usize;
        let mut visited = HashSet::new();
        visited.insert(node.node_id);

        while let Some(pid) = cursor {
            if hops >= 12 || !visited.insert(pid) {
                break;
            }
            let Some(parent) = index.nodes_by_id.get(&pid) else {
                break;
            };
            if parent.clickable {
                return true;
            }
            cursor = parent.parent_id;
            hops += 1;
        }
        false
    }

    fn is_ignored_node(node: &ShadowUINode, ignored_packages: &HashSet<String>) -> bool {
        if !node.package_name.is_empty() && ignored_packages.contains(&node.package_name) {
            return true;
        }
        if !node.resource_id.is_empty() {
            for pkg in ignored_packages {
                if node.resource_id.starts_with(pkg)
                    && node.resource_id.as_bytes().get(pkg.len()) == Some(&b':')
                {
                    return true;
                }
            }
        }
        false
    }

    fn extract_prefix_and_leaf_candidates(query: &str) -> Vec<(String, String, bool)> {
        for sep in ["->", ">", "»", " / ", " | "] {
            if query.contains(sep) {
                let parts: Vec<&str> = query
                    .split(sep)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.len() >= 2 {
                    let mut out = Vec::new();
                    let leaf = parts[parts.len() - 1].to_string();
                    for idx in (0..parts.len() - 1).rev() {
                        out.push((parts[idx].to_string(), leaf.clone(), true));
                    }
                    return out;
                }
            }
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        if words.len() >= 2 {
            let mut out = Vec::new();
            for split_at in (1..words.len()).rev() {
                let prefix = words[..split_at].join(" ");
                let leaf = words[split_at..].join(" ");
                if !prefix.is_empty() && !leaf.is_empty() {
                    out.push((prefix, leaf, false));
                }
            }
            return out;
        }
        Vec::new()
    }
}

enum BackToRootResult {
    ArrivedAtGoal { backs_used: usize, final_hash: u64 },
    FoundRoute { backs_used: usize },
    NotRecovered,
}
