use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::adb::{DroidPathActionExecutor, DroidPathAdbRunner};
use crate::bridge::{
    ExploreOptions, ExploreOutcome, RouteExecuteOptions, ShadowGraphBridge, ShadowNavigationOutcome,
};
use crate::protocol::{ShadowCompactNode, ShadowTreeMatchMode, ShadowUINode};
use crate::router::{MacroActionExecutor, PathDeviationError};
use crate::shadow_dom::ShadowDOMUpdate;
use crate::spatial_graph::{
    AttributedUIAction, ScreenLayoutHasher, SeedGraphCatalog, SpatialGraphEngine,
};
use crate::transport::{DaemonConnectionState, DaemonEndpointConfiguration};

pub type PortForwarderHook =
    Arc<dyn Fn(Option<String>, u16) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
pub type DaemonLauncherHook = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync,
>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroidPathGraphStats {
    pub vertex_count: usize,
    pub seed_vertex_count: usize,
    pub hydrated_vertex_count: usize,
    pub edge_count: usize,
    pub current_state_hash: Option<u64>,
    pub current_package: String,
    pub connection_state: DaemonConnectionState,
    pub has_fresh_shadow_dom: bool,
    pub storage_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDeployReport {
    pub serial: String,
    pub port: u16,
    pub jar_path: Option<String>,
    pub sha256: Option<String>,
    pub pushed: bool,
    pub ready: bool,
}

struct PersistenceInner {
    initialized: bool,
    save_task: Option<JoinHandle<()>>,
    last_saved_data: Option<Vec<u8>>,
}

/// Debounced atomic persistence coordinator for `~/.droidpath/shadow_graph.json`.
pub struct DroidPathPersistenceCoordinator {
    storage_path: PathBuf,
    seed_default_graph: bool,
    inner: Mutex<PersistenceInner>,
}

impl DroidPathPersistenceCoordinator {
    pub fn new(storage_path: PathBuf) -> Self {
        Self::new_with_seed(storage_path, false)
    }

    pub fn new_with_seed(storage_path: PathBuf, seed_default_graph: bool) -> Self {
        Self {
            storage_path,
            seed_default_graph,
            inner: Mutex::new(PersistenceInner {
                initialized: false,
                save_task: None,
                last_saved_data: None,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.storage_path
    }

    pub fn path_string(&self) -> String {
        self.storage_path.to_string_lossy().into_owned()
    }

    pub fn default_storage_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".droidpath")
            .join("shadow_graph.json")
    }

    pub async fn ensure_initialized(self: &Arc<Self>, engine: Arc<SpatialGraphEngine>) {
        {
            let mut guard = self.inner.lock().await;
            if guard.initialized {
                return;
            }
            guard.initialized = true;
        }

        if self.storage_path.exists() {
            match tokio::fs::read(&self.storage_path).await {
                Ok(data) => match engine.import_graph_json(&data).await {
                    Ok(()) => {
                        self.inner.lock().await.last_saved_data = Some(data);
                    }
                    Err(err) => {
                        DroidPathManager::log_stderr(&format!(
                            "Failed to parse persisted graph at {}: {err}",
                            self.storage_path.display()
                        ));
                    }
                },
                Err(err) => {
                    DroidPathManager::log_stderr(&format!(
                        "Failed to read persisted graph at {}: {err}",
                        self.storage_path.display()
                    ));
                }
            }
        }

        if self.seed_default_graph
            && std::env::var("DROIDPATH_DISABLE_SEED_GRAPH").as_deref() != Ok("1")
        {
            let _ = engine
                .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
                .await;
        }

        let weak_self = Arc::downgrade(self);
        let engine_for_cb = Arc::clone(&engine);
        engine
            .set_on_topology_changed(Some(Arc::new(move || {
                let weak = weak_self.clone();
                let eng = Arc::clone(&engine_for_cb);
                Box::pin(async move {
                    if let Some(coord) = weak.upgrade() {
                        coord.schedule_save(eng, Duration::from_millis(1500)).await;
                    }
                })
            })))
            .await;
    }

    pub async fn schedule_save(
        self: &Arc<Self>,
        engine: Arc<SpatialGraphEngine>,
        debounce: Duration,
    ) {
        let mut guard = self.inner.lock().await;
        if let Some(prev) = guard.save_task.take() {
            prev.abort();
        }
        let coord = Arc::clone(self);
        guard.save_task = Some(tokio::spawn(async move {
            tokio::time::sleep(debounce).await;
            coord.write_snapshot_if_changed(&engine).await;
        }));
    }

    pub async fn flush(&self, engine: &SpatialGraphEngine) {
        {
            let mut guard = self.inner.lock().await;
            if let Some(prev) = guard.save_task.take() {
                prev.abort();
            }
        }
        self.write_snapshot_if_changed(engine).await;
    }

    async fn write_snapshot_if_changed(&self, engine: &SpatialGraphEngine) {
        let v_count = engine.vertex_count().await;
        let e_count = engine.edge_count().await;
        if v_count == 0 && e_count == 0 {
            return;
        }
        // Avoid writing an untouched pure-seed graph to disk before any real device activity occurs.
        if self.seed_default_graph
            && engine.topology_revision().await == 0
            && engine.hydrated_vertex_count().await == 0
            && !self.storage_path.exists()
        {
            return;
        }

        let data = match engine.export_graph_json(false).await {
            Ok(d) => d,
            Err(err) => {
                DroidPathManager::log_stderr(&format!(
                    "Failed to serialize graph for {}: {err}",
                    self.storage_path.display()
                ));
                return;
            }
        };

        {
            let guard = self.inner.lock().await;
            if guard.last_saved_data.as_deref() == Some(data.as_slice()) {
                return;
            }
        }

        if let Some(parent) = self.storage_path.parent() {
            if let Err(err) = tokio::fs::create_dir_all(parent).await {
                DroidPathManager::log_stderr(&format!(
                    "Failed to create graph directory {}: {err}",
                    parent.display()
                ));
                return;
            }
        }

        // Cross-platform atomic write: temp file in same directory + rename
        let tmp_name = format!(
            ".shadow_graph.{}.tmp",
            Uuid::new_v4().simple()
        );
        let tmp_path = self
            .storage_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(tmp_name);

        if let Err(err) = tokio::fs::write(&tmp_path, &data).await {
            DroidPathManager::log_stderr(&format!(
                "Failed to write temp graph file {}: {err}",
                tmp_path.display()
            ));
            return;
        }

        #[cfg(windows)]
        {
            // Windows rename fails if destination exists; remove destination first if needed.
            if self.storage_path.exists() {
                let _ = tokio::fs::remove_file(&self.storage_path).await;
            }
        }

        if let Err(err) = tokio::fs::rename(&tmp_path, &self.storage_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            DroidPathManager::log_stderr(&format!(
                "Failed to atomically move graph into {}: {err}",
                self.storage_path.display()
            ));
            return;
        }

        self.inner.lock().await.last_saved_data = Some(data);
    }
}

struct ManagerState {
    has_started: bool,
}

/// Standalone cross-platform lifecycle coordinator for `ShadowGraphBridge`,
/// persistent Spatial Graph JSON, idempotent SHA-256 `android-daemon.jar` ADB deploy,
/// and self-healing daemon recovery.
pub struct DroidPathManager {
    pub bridge: Arc<ShadowGraphBridge>,
    persistence: Arc<DroidPathPersistenceCoordinator>,
    lifecycle_gate: Mutex<()>,
    state: Mutex<ManagerState>,
    active_serial: Arc<RwLock<Option<String>>>,
    explicit_daemon_jar_path: Option<PathBuf>,
    adb_runner: DroidPathAdbRunner,
    port_forwarder: Option<PortForwarderHook>,
    daemon_launcher: Option<DaemonLauncherHook>,
}

impl DroidPathManager {
    pub fn new(
        port: u16,
        storage_path: PathBuf,
        daemon_jar_path: Option<PathBuf>,
        adb_runner: DroidPathAdbRunner,
        port_forwarder: Option<PortForwarderHook>,
        daemon_launcher: Option<DaemonLauncherHook>,
    ) -> Arc<Self> {
        Self::new_with_seed(
            port,
            storage_path,
            daemon_jar_path,
            adb_runner,
            port_forwarder,
            daemon_launcher,
            false,
        )
    }

    pub fn new_with_seed(
        port: u16,
        storage_path: PathBuf,
        daemon_jar_path: Option<PathBuf>,
        adb_runner: DroidPathAdbRunner,
        port_forwarder: Option<PortForwarderHook>,
        daemon_launcher: Option<DaemonLauncherHook>,
        seed_default_graph: bool,
    ) -> Arc<Self> {
        let bridge = Arc::new(ShadowGraphBridge::new(
            DaemonEndpointConfiguration::with_port(port),
            ScreenLayoutHasher::default(),
            0.72,
        ));
        let manager = Arc::new(Self {
            bridge: Arc::clone(&bridge),
            persistence: Arc::new(DroidPathPersistenceCoordinator::new_with_seed(
                storage_path,
                seed_default_graph,
            )),
            lifecycle_gate: Mutex::new(()),
            state: Mutex::new(ManagerState { has_started: false }),
            active_serial: Arc::new(RwLock::new(None)),
            explicit_daemon_jar_path: daemon_jar_path,
            adb_runner,
            port_forwarder,
            daemon_launcher,
        });

        let weak_mgr = Arc::downgrade(&manager);
        bridge.set_daemon_recovery_hook(Some(Arc::new(move |serial, port| {
            let weak = weak_mgr.clone();
            Box::pin(async move {
                if let Some(mgr) = weak.upgrade() {
                    mgr.recover_daemon_and_await_stream(&serial, port).await
                } else {
                    false
                }
            })
        })));

        manager
    }

    pub fn adb_runner(&self) -> &DroidPathAdbRunner {
        &self.adb_runner
    }

    pub fn action_executor(&self) -> Arc<DroidPathActionExecutor> {
        Arc::new(DroidPathActionExecutor::with_daemon(
            self.adb_runner.clone(),
            Arc::clone(&self.bridge.client),
            Some(Arc::clone(&self.active_serial)),
        ))
    }

    pub fn storage_path(&self) -> &Path {
        self.persistence.path()
    }

    pub async fn active_serial(&self) -> Option<String> {
        self.active_serial.read().await.clone()
    }

    pub async fn load_persisted_graph_if_needed(&self) {
        self.persistence
            .ensure_initialized(Arc::clone(&self.bridge.graph_engine))
            .await;
    }

    pub async fn flush_persisted_graph(&self) {
        self.persistence.flush(&self.bridge.graph_engine).await;
    }

    pub async fn ensure_started(
        self: &Arc<Self>,
        serial: Option<&str>,
        force_reconnect_if_waiting: bool,
    ) {
        self.load_persisted_graph_if_needed().await;
        let _gate = self.lifecycle_gate.lock().await;

        let valid_serial = serial
            .filter(|s| !s.is_empty() && DroidPathAdbRunner::validate_serial(s))
            .map(ToString::to_string);

        let (should_configure_and_start, serial_changed, acquired_initial_serial) = {
            let mut st = self.state.lock().await;
            let mut active = self.active_serial.write().await;
            let prev = active.clone();
            if let Some(ref vs) = valid_serial {
                *active = Some(vs.clone());
            }
            if !st.has_started {
                st.has_started = true;
                (true, false, false)
            } else {
                let changed = valid_serial.is_some() && prev.is_some() && prev != valid_serial;
                let acquired = valid_serial.is_some() && prev.is_none();
                (false, changed, acquired)
            }
        };

        let port = self.bridge.client.configuration().port;

        if should_configure_and_start {
            let weak_self = Arc::downgrade(self);
            self.bridge
                .set_before_reconnect_hook(Some(Arc::new(move |_| {
                    let weak = weak_self.clone();
                    Box::pin(async move {
                        if let Some(mgr) = weak.upgrade() {
                            mgr.forward_daemon_port(port, None).await;
                        }
                    })
                })))
                .await;
            self.forward_daemon_port(port, None).await;
            self.bridge.start().await;
        } else {
            let conn_state = self.bridge.client.state().await;
            match conn_state {
                DaemonConnectionState::Stopped | DaemonConnectionState::Disconnected => {
                    self.forward_daemon_port(port, None).await;
                    self.bridge.start().await;
                }
                DaemonConnectionState::WaitingToReconnect { .. } => {
                    if serial_changed || acquired_initial_serial || force_reconnect_if_waiting {
                        self.bridge.reconnect().await;
                    }
                }
                DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. } => {
                    if serial_changed || (acquired_initial_serial && !conn_state.is_connected()) {
                        self.bridge.reconnect().await;
                    }
                }
            }
        }
    }

    pub async fn stop(&self) {
        let pending_index = if self.bridge.dom_state.has_session_snapshot().await {
            self.bridge.dom_state.current_index().await
        } else {
            None
        };

        {
            let _gate = self.lifecycle_gate.lock().await;
            self.state.lock().await.has_started = false;
            self.bridge.stop().await;
        }

        if let Some(idx) = pending_index {
            let fp = self.bridge.hasher.fingerprint(&idx);
            let cur_hash = self.bridge.graph_engine.current_state_hash().await;
            if cur_hash != Some(fp.layout_hash)
                && (!fp.tokens.is_empty() || !idx.ordered_nodes.is_empty())
            {
                let _ = self
                    .bridge
                    .graph_engine
                    .process(&ShadowDOMUpdate {
                        index: Arc::clone(&idx),
                        is_full_snapshot: true,
                        inserted_count: idx.ordered_nodes.len(),
                        updated_count: 0,
                        removed_count: 0,
                        event_type: None,
                        event_package: None,
                        is_secure_screen_active: false,
                    })
                    .await;
            }
        }
        self.flush_persisted_graph().await;
    }

    pub async fn graph_stats(&self) -> DroidPathGraphStats {
        self.load_persisted_graph_if_needed().await;
        let v_count = self.bridge.graph_engine.vertex_count().await;
        let seed_count = self.bridge.graph_engine.seed_vertex_count().await;
        let hydrated_count = self.bridge.graph_engine.hydrated_vertex_count().await;
        let e_count = self.bridge.graph_engine.edge_count().await;
        let hash = self.bridge.graph_engine.current_state_hash().await;
        let pkg = self
            .bridge
            .dom_state
            .current_index()
            .await
            .map(|i| i.active_package.clone())
            .unwrap_or_default();
        let conn = self.bridge.client.state().await;
        let fresh = self.bridge.dom_state.is_fresh(15.0).await;
        DroidPathGraphStats {
            vertex_count: v_count,
            seed_vertex_count: seed_count,
            hydrated_vertex_count: hydrated_count,
            edge_count: e_count,
            current_state_hash: hash,
            current_package: pkg,
            connection_state: conn,
            has_fresh_shadow_dom: fresh,
            storage_path: self.persistence.path_string(),
        }
    }

    pub async fn fast_compact_tree_if_fresh(
        &self,
        max_wait_secs: f64,
        max_age_secs: f64,
    ) -> Option<Vec<ShadowCompactNode>> {
        self.bridge
            .fast_compact_tree_if_fresh(max_wait_secs, max_age_secs)
            .await
    }

    pub async fn find_node_by_text(
        &self,
        query: &str,
        prefer_clickable: bool,
    ) -> Option<ShadowUINode> {
        if !self.bridge.dom_state.is_fresh(15.0).await {
            return None;
        }
        self.bridge
            .find_node_by_text(query, ShadowTreeMatchMode::ContainsCaseInsensitive, prefer_clickable)
            .await
    }

    pub async fn find_node_by_resource_id(&self, resource_id: &str) -> Option<ShadowUINode> {
        if !self.bridge.dom_state.is_fresh(15.0).await {
            return None;
        }
        self.bridge
            .find_node_by_resource_id(resource_id, ShadowTreeMatchMode::SuffixOrExactResourceId)
            .await
    }

    pub async fn find_node_by_content_desc(&self, query: &str) -> Option<ShadowUINode> {
        if !self.bridge.dom_state.is_fresh(15.0).await {
            return None;
        }
        self.bridge
            .find_node_by_content_desc(query, ShadowTreeMatchMode::ContainsCaseInsensitive)
            .await
    }

    pub async fn record_action(&self, action: AttributedUIAction) {
        self.bridge.record_action(action).await;
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn navigate_to_node(
        self: &Arc<Self>,
        serial: &str,
        target: Option<String>,
        target_hash: Option<u64>,
        max_steps: usize,
        step_timeout_ms: u64,
        confirmed: bool,
        auto_explore: bool,
        secure_handoff_timeout_secs: f64,
        executor: Option<Arc<dyn MacroActionExecutor>>,
    ) -> Result<ShadowNavigationOutcome, PathDeviationError> {
        self.load_persisted_graph_if_needed().await;
        self.ensure_started(Some(serial), true).await;
        let active_executor =
            executor.unwrap_or_else(|| self.action_executor() as Arc<dyn MacroActionExecutor>);
        let outcome = self
            .bridge
            .route_and_execute(
                serial,
                active_executor,
                RouteExecuteOptions {
                    target_hint: target,
                    target_hash,
                    max_steps,
                    step_timeout_ms,
                    confirmed,
                    auto_explore,
                    secure_handoff_timeout_secs,
                    ..RouteExecuteOptions::default()
                },
            )
            .await;
        if matches!(
            outcome,
            Ok(ShadowNavigationOutcome::Arrived(_)) | Ok(ShadowNavigationOutcome::Explored(_))
        ) {
            self.flush_persisted_graph().await;
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn explore_and_click(
        self: &Arc<Self>,
        serial: &str,
        target: &str,
        max_scrolls: usize,
        step_timeout_ms: u64,
        confirmed: bool,
        prefer_graph_route: bool,
        secure_handoff_timeout_secs: f64,
        executor: Option<Arc<dyn MacroActionExecutor>>,
    ) -> Result<ExploreOutcome, PathDeviationError> {
        self.load_persisted_graph_if_needed().await;
        self.ensure_started(Some(serial), true).await;
        let active_executor =
            executor.unwrap_or_else(|| self.action_executor() as Arc<dyn MacroActionExecutor>);
        let outcome = self
            .bridge
            .explore_and_click(
                target,
                serial,
                active_executor,
                ExploreOptions {
                    max_scrolls,
                    step_timeout_ms,
                    confirmed,
                    prefer_graph_route,
                    secure_handoff_timeout_secs,
                    ..ExploreOptions::default()
                },
            )
            .await;
        if matches!(
            outcome,
            Ok(ExploreOutcome::ExploredAndClicked { .. })
                | Ok(ExploreOutcome::ArrivedViaGraph { .. })
        ) {
            self.flush_persisted_graph().await;
        }
        outcome
    }

    pub async fn forward_daemon_port(&self, port: u16, explicit_serial: Option<&str>) {
        let target_serial = match explicit_serial {
            Some(s) => Some(s.to_string()),
            None => self.active_serial.read().await.clone(),
        };
        if let Some(ref forwarder) = self.port_forwarder {
            forwarder(target_serial, port).await;
            return;
        }
        let spec = format!("tcp:{port}");
        let args: Vec<&str> = if let Some(ref s) = target_serial {
            if !s.is_empty() {
                vec!["-s", s.as_str(), "forward", &spec, &spec]
            } else {
                vec!["forward", &spec, &spec]
            }
        } else {
            vec!["forward", &spec, &spec]
        };
        let _ = self.adb_runner.execute(&args, 1.5).await;
    }

    pub async fn recover_daemon_and_await_stream(
        self: &Arc<Self>,
        serial: &str,
        port: u16,
    ) -> bool {
        if !DroidPathAdbRunner::validate_serial(serial) {
            return false;
        }
        self.ensure_started(Some(serial), false).await;

        if self.bridge.ensure_shadow_stream_ready(0.25, 15.0).await
            && self.bridge.dom_state.current_index().await.is_some()
        {
            return true;
        }

        let launch_res = if let Some(ref launcher) = self.daemon_launcher {
            launcher(serial.to_string(), port).await
        } else {
            self.deploy_and_launch_daemon(serial, port)
                .await
                .map(|_| ())
        };

        if let Err(err) = launch_res {
            Self::log_stderr(&format!(
                "Daemon auto-recovery launch failed for {serial}: {err}"
            ));
            return false;
        }

        self.forward_daemon_port(port, Some(serial)).await;
        if self.bridge.client.state().await == DaemonConnectionState::Stopped {
            self.bridge.start().await;
        } else {
            self.bridge.reconnect().await;
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let st = self.bridge.client.state().await;
            if st == DaemonConnectionState::Stopped {
                return false;
            }
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .clamp(0.1, 1.0);
            if self.bridge.ensure_shadow_stream_ready(slice, 15.0).await
                && self.bridge.dom_state.current_index().await.is_some()
            {
                return true;
            }
            if matches!(
                st,
                DaemonConnectionState::WaitingToReconnect { .. }
                    | DaemonConnectionState::Disconnected
            ) {
                tokio::time::sleep(Duration::from_millis(150)).await;
                self.forward_daemon_port(port, Some(serial)).await;
                self.bridge.reconnect().await;
            }
        }

        self.bridge.ensure_shadow_stream_ready(0.1, 15.0).await
            && self.bridge.dom_state.current_index().await.is_some()
    }

    pub fn resolve_daemon_jar_path(&self) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(ref explicit) = self.explicit_daemon_jar_path {
            candidates.push(explicit.clone());
        }
        if let Ok(env_jar) = std::env::var("DROIDPATH_DAEMON_JAR") {
            if !env_jar.is_empty() {
                candidates.push(PathBuf::from(env_jar));
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                candidates.push(exe_dir.join("android-daemon.jar"));
                candidates.push(exe_dir.join("resources").join("android-daemon.jar"));
                candidates.push(exe_dir.join("../share/droidpath/android-daemon.jar"));
                candidates.push(exe_dir.join("../share/android-daemon.jar"));
                candidates.push(exe_dir.join("../Resources/bin/android-daemon.jar"));
                candidates.push(exe_dir.join("../Resources/android-daemon.jar"));
            }
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(
                home.join(".droidpath")
                    .join("share")
                    .join("android-daemon.jar"),
            );
            candidates.push(home.join(".droidpath").join("android-daemon.jar"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join("android-daemon/build/dist/android-daemon.jar"));
            candidates.push(cwd.join("packages/droidpath-cli/assets/android-daemon.jar"));
            candidates.push(cwd.join("android-daemon.jar"));
        }
        #[cfg(not(windows))]
        {
            candidates.push(PathBuf::from("/usr/local/share/droidpath/android-daemon.jar"));
            candidates.push(PathBuf::from(
                "/opt/homebrew/share/droidpath/android-daemon.jar",
            ));
        }

        candidates.into_iter().find(|p| p.is_file())
    }

    pub async fn deploy_and_launch_daemon(
        &self,
        serial: &str,
        port: u16,
    ) -> Result<DaemonDeployReport, String> {
        if !DroidPathAdbRunner::validate_serial(serial) {
            return Err(format!("Invalid ADB device serial '{serial}'."));
        }
        let port = if port == 0 {
            DroidPathAdbRunner::allocate_ephemeral_port()
                .map_err(|e| format!("Failed to allocate ephemeral daemon port: {e}"))?
        } else {
            port
        };
        let headless = std::env::var("DROIDPATH_HEADLESS").as_deref() == Ok("1");
        if headless {
            let _ = self.adb_runner.wait_for_boot_completed(serial, 30.0).await;
        }
        let remote_jar = "/data/local/tmp/android-daemon.jar";
        let remote_log = "/data/local/tmp/android-daemon.log";

        let mut pushed = false;
        let mut sha_hex = None;
        let local_jar = self.resolve_daemon_jar_path();

        if let Some(ref jar_path) = local_jar {
            let jar_data = tokio::fs::read(jar_path)
                .await
                .map_err(|e| format!("Failed to read {}: {e}", jar_path.display()))?;
            let digest = Sha256::digest(&jar_data);
            let local_sha = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
            sha_hex = Some(local_sha.clone());

            let sha_cmd = format!(
                "sha256sum '{remote_jar}' 2>/dev/null || toybox sha256sum '{remote_jar}' 2>/dev/null"
            );
            let remote_sha_raw = self
                .adb_runner
                .execute(&["-s", serial, "shell", &sha_cmd], 3.0)
                .await
                .unwrap_or_default();
            let remote_sha = remote_sha_raw
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim();

            if remote_sha != local_sha {
                let jar_str = jar_path.to_string_lossy();
                self.adb_runner
                    .execute(&["-s", serial, "push", &jar_str, remote_jar], 12.0)
                    .await
                    .map_err(|e| format!("adb push failed: {e}"))?;
                let _ = self
                    .adb_runner
                    .execute(&["-s", serial, "shell", "chmod", "644", remote_jar], 3.0)
                    .await;
                pushed = true;
            }
        } else {
            let exists_cmd = format!("test -f '{remote_jar}' && echo OK");
            let exists_out = self
                .adb_runner
                .execute(&["-s", serial, "shell", &exists_cmd], 3.0)
                .await
                .unwrap_or_default();
            if !exists_out.contains("OK") {
                return Err(format!(
                    "android-daemon.jar not found locally or at {remote_jar}."
                ));
            }
        }

        let kill_cmd = "pkill -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true; \
            i=0; while [ $i -lt 15 ]; do pgrep -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || break; sleep 0.05; i=$((i+1)); done; \
            pkill -9 -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true";
        let _ = self
            .adb_runner
            .execute(&["-s", serial, "shell", kill_cmd], 4.0)
            .await;

        let extra_flags = if headless {
            " --wait-boot --connect-retries=12"
        } else {
            " --connect-retries=8"
        };
        let daemon_cmd = format!(
            "env CLASSPATH='{remote_jar}' app_process / com.shadowgraph.daemon.DaemonMain \
             --port={port} --debounce-ms=20 --max-depth=48 --heartbeat-ms=5000 --no-stdin-watchdog{extra_flags}"
        );
        let launch_cmd = format!(
            "rm -f '{remote_log}'; \
             if command -v setsid >/dev/null 2>&1; then \
               (setsid {daemon_cmd} </dev/null >'{remote_log}' 2>&1 &) </dev/null >/dev/null 2>&1; \
             else \
               (nohup {daemon_cmd} </dev/null >'{remote_log}' 2>&1 &) </dev/null >/dev/null 2>&1; \
             fi"
        );
        self.adb_runner
            .execute(&["-s", serial, "shell", &launch_cmd], 4.0)
            .await
            .map_err(|e| format!("Failed to launch android-daemon: {e}"))?;

        let ready_deadline = Instant::now() + Duration::from_secs(if headless { 10 } else { 4 });
        let check_cmd = format!(
            "if grep -q 'SHADOWGRAPH_DAEMON_READY' '{remote_log}' 2>/dev/null; then echo READY; \
             elif grep -q 'Fatal startup error' '{remote_log}' 2>/dev/null; then echo FAILED; fi"
        );
        while Instant::now() < ready_deadline {
            let out = self
                .adb_runner
                .execute(&["-s", serial, "shell", &check_cmd], 2.0)
                .await
                .unwrap_or_default();
            if out.contains("READY") {
                self.forward_daemon_port(port, Some(serial)).await;
                return Ok(DaemonDeployReport {
                    serial: serial.to_string(),
                    port,
                    jar_path: local_jar.map(|p| p.to_string_lossy().into_owned()),
                    sha256: sha_hex,
                    pushed,
                    ready: true,
                });
            }
            if out.contains("FAILED") {
                let err_log = self
                    .adb_runner
                    .execute(
                        &["-s", serial, "shell", &format!("cat '{remote_log}' 2>/dev/null")],
                        2.0,
                    )
                    .await
                    .unwrap_or_default();
                return Err(format!("android-daemon failed to start: {err_log}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let tail_log = self
            .adb_runner
            .execute(
                &["-s", serial, "shell", &format!("cat '{remote_log}' 2>/dev/null")],
                2.0,
            )
            .await
            .unwrap_or_default();
        let suffix = if tail_log.trim().is_empty() {
            String::new()
        } else {
            format!(" Log: {}", tail_log.trim())
        };
        Err(format!(
            "Timed out waiting for SHADOWGRAPH_DAEMON_READY.{suffix}"
        ))
    }

    pub async fn stop_remote_daemon(&self, serial: &str, port: u16) -> Result<(), String> {
        if !DroidPathAdbRunner::validate_serial(serial) {
            return Err(format!("Invalid ADB device serial '{serial}'."));
        }
        let kill_cmd = "pkill -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true; \
            pkill -9 -f '[c]om.shadowgraph.daemon.DaemonMain' >/dev/null 2>&1 || true";
        let _ = self
            .adb_runner
            .execute(&["-s", serial, "shell", kill_cmd], 4.0)
            .await;
        let spec = format!("tcp:{port}");
        let _ = self
            .adb_runner
            .execute(&["-s", serial, "forward", "--remove", &spec], 2.0)
            .await;
        Ok(())
    }

    pub fn log_stderr(message: &str) {
        eprintln!("[DroidPath] {message}");
    }
}
