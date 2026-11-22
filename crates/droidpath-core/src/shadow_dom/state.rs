use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Notify, RwLock};

use crate::protocol::{
    ShadowCompactNode, ShadowPatchMask, ShadowTreeMatchMode, ShadowUINode, WireErrorPayload,
    WireHeartbeatPayload, WireHelloPayload, WireMessage, WireNodePatch, WireSecureScreenPayload,
};

use super::index::{ShadowDOMUpdate, ShadowTreeIndex};

pub type ResyncHandler = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMetadata {
    pub seq: u64,
    pub package: String,
    pub window_id: i32,
    pub age_ms: u64,
}

struct DomInner {
    current_index: Option<Arc<ShadowTreeIndex>>,
    last_hello: Option<WireHelloPayload>,
    last_heartbeat: Option<WireHeartbeatPayload>,
    last_error: Option<WireErrorPayload>,
    last_received_at: Option<Instant>,
    is_disconnected: bool,
    has_session_snapshot: bool,
    resync_request_count: u64,
    is_secure_screen_active: bool,
    secure_screen_reason: Option<String>,
    secure_screen_package: String,
    last_resync_requested_at: Option<Instant>,
}

/// Thread-safe source of truth holding the absolute latest Android Shadow DOM.
/// All reads execute purely in memory without ever spawning ADB.
pub struct ShadowDomState {
    inner: RwLock<DomInner>,
    resync_cooldown: Duration,
    resync_handler: Option<ResyncHandler>,
    update_tx: broadcast::Sender<ShadowDOMUpdate>,
    secure_tx: broadcast::Sender<WireSecureScreenPayload>,
    snapshot_notify: Notify,
    secure_cleared_notify: Notify,
}

impl ShadowDomState {
    pub fn new(resync_cooldown_secs: f64, resync_handler: Option<ResyncHandler>) -> Self {
        let (update_tx, _) = broadcast::channel(256);
        let (secure_tx, _) = broadcast::channel(64);
        Self {
            inner: RwLock::new(DomInner {
                current_index: None,
                last_hello: None,
                last_heartbeat: None,
                last_error: None,
                last_received_at: None,
                is_disconnected: true,
                has_session_snapshot: false,
                resync_request_count: 0,
                is_secure_screen_active: false,
                secure_screen_reason: None,
                secure_screen_package: String::new(),
                last_resync_requested_at: None,
            }),
            resync_cooldown: Duration::from_secs_f64(resync_cooldown_secs.max(0.0)),
            resync_handler,
            update_tx,
            secure_tx,
            snapshot_notify: Notify::new(),
            secure_cleared_notify: Notify::new(),
        }
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<ShadowDOMUpdate> {
        self.update_tx.subscribe()
    }

    pub fn subscribe_secure_screen(&self) -> broadcast::Receiver<WireSecureScreenPayload> {
        self.secure_tx.subscribe()
    }

    pub async fn handle_message(&self, message: WireMessage) {
        let now = Instant::now();

        match message {
            WireMessage::Hello(payload) => {
                let mut inner = self.inner.write().await;
                inner.last_hello = Some(payload);
                inner.has_session_snapshot = false;
            }

            WireMessage::Snapshot(snap) => {
                let index = Arc::new(ShadowTreeIndex::new(
                    snap.seq,
                    snap.timestamp_ms,
                    snap.active_package.clone(),
                    snap.active_window_id,
                    snap.nodes,
                ));
                let node_count = index.ordered_nodes.len();
                let mut secure_cleared_event = None;
                let is_secure_active;

                {
                    let mut inner = self.inner.write().await;
                    inner.last_received_at = Some(now);
                    inner.is_disconnected = false;
                    inner.has_session_snapshot = true;
                    inner.current_index = Some(Arc::clone(&index));

                    if let Some(explicit_secure) = snap.secure_screen_active {
                        if explicit_secure && !inner.is_secure_screen_active {
                            inner.is_secure_screen_active = true;
                            inner.secure_screen_reason = Some(if node_count == 0 {
                                "flag_secure_null_root".to_string()
                            } else {
                                "biometric_prompt".to_string()
                            });
                            inner.secure_screen_package = snap.active_package.clone();
                        } else if !explicit_secure && inner.is_secure_screen_active {
                            inner.is_secure_screen_active = false;
                            inner.secure_screen_reason = None;
                            inner.secure_screen_package = snap.active_package.clone();
                            secure_cleared_event = Some(WireSecureScreenPayload {
                                code: "SECURE_SCREEN_CLEARED".to_string(),
                                active: false,
                                reason: "tree_restored".to_string(),
                                active_package: snap.active_package.clone(),
                                timestamp_ms: snap.timestamp_ms,
                            });
                        }
                    } else if node_count > 0 && inner.is_secure_screen_active {
                        inner.is_secure_screen_active = false;
                        inner.secure_screen_reason = None;
                        inner.secure_screen_package = snap.active_package.clone();
                        secure_cleared_event = Some(WireSecureScreenPayload {
                            code: "SECURE_SCREEN_CLEARED".to_string(),
                            active: false,
                            reason: "tree_restored".to_string(),
                            active_package: snap.active_package.clone(),
                            timestamp_ms: snap.timestamp_ms,
                        });
                    }
                    is_secure_active = inner.is_secure_screen_active;
                }

                self.snapshot_notify.notify_waiters();
                if let Some(ev) = secure_cleared_event {
                    let _ = self.secure_tx.send(ev);
                    self.secure_cleared_notify.notify_waiters();
                }

                let _ = self.update_tx.send(ShadowDOMUpdate {
                    index,
                    is_full_snapshot: true,
                    inserted_count: node_count,
                    updated_count: 0,
                    removed_count: 0,
                    event_type: None,
                    event_package: None,
                    is_secure_screen_active: is_secure_active,
                });
            }

            WireMessage::Diff(diff) => {
                let mut need_resync = false;
                let mut secure_cleared_event = None;
                let mut emitted_update = None;

                {
                    let mut inner = self.inner.write().await;
                    inner.last_received_at = Some(now);
                    inner.is_disconnected = false;

                    let Some(current) = inner.current_index.clone() else {
                        need_resync = true;
                        drop(inner);
                        if need_resync {
                            self.trigger_resync_if_needed(now).await;
                        }
                        return;
                    };

                    if diff.seq <= current.seq {
                        return;
                    }
                    if diff.base_seq != current.seq {
                        need_resync = true;
                    } else {
                        let mut next_by_id = current.nodes_by_id.clone();
                        for removed_id in &diff.removed {
                            next_by_id.remove(removed_id);
                        }

                        let mut diverged = false;
                        for patch in &diff.updated {
                            let Some(existing) = next_by_id.get_mut(&patch.node_id) else {
                                diverged = true;
                                break;
                            };
                            Self::apply_patch(patch, existing);
                        }

                        if diverged {
                            need_resync = true;
                        } else {
                            let inserted_count = diff.inserted.len();
                            let updated_count = diff.updated.len();
                            let removed_count = diff.removed.len();
                            for inserted in diff.inserted {
                                next_by_id.insert(inserted.node_id, inserted);
                            }

                            let next_index = Arc::new(ShadowTreeIndex::new(
                                diff.seq,
                                diff.timestamp_ms,
                                diff.active_package.clone(),
                                diff.active_window_id,
                                next_by_id.into_values().collect(),
                            ));
                            let has_nodes = !next_index.ordered_nodes.is_empty();
                            inner.current_index = Some(Arc::clone(&next_index));
                            inner.has_session_snapshot = true;

                            if has_nodes && inner.is_secure_screen_active {
                                inner.is_secure_screen_active = false;
                                inner.secure_screen_reason = None;
                                inner.secure_screen_package = diff.active_package.clone();
                                secure_cleared_event = Some(WireSecureScreenPayload {
                                    code: "SECURE_SCREEN_CLEARED".to_string(),
                                    active: false,
                                    reason: "tree_restored".to_string(),
                                    active_package: diff.active_package.clone(),
                                    timestamp_ms: diff.timestamp_ms,
                                });
                            }

                            emitted_update = Some(ShadowDOMUpdate {
                                index: next_index,
                                is_full_snapshot: false,
                                inserted_count,
                                updated_count,
                                removed_count,
                                event_type: diff.event_type,
                                event_package: diff.event_package,
                                is_secure_screen_active: inner.is_secure_screen_active,
                            });
                        }
                    }
                }

                if need_resync {
                    self.trigger_resync_if_needed(now).await;
                    return;
                }

                self.snapshot_notify.notify_waiters();
                if let Some(ev) = secure_cleared_event {
                    let _ = self.secure_tx.send(ev);
                    self.secure_cleared_notify.notify_waiters();
                }
                if let Some(update) = emitted_update {
                    let _ = self.update_tx.send(update);
                }
            }

            WireMessage::Heartbeat(hb) => {
                let mut inner = self.inner.write().await;
                inner.last_heartbeat = Some(hb);
                if inner.has_session_snapshot {
                    inner.last_received_at = Some(now);
                    inner.is_disconnected = false;
                }
            }

            WireMessage::Error(err) => {
                self.inner.write().await.last_error = Some(err);
            }

            WireMessage::Pong(_) | WireMessage::InputResult(_) => {
                let mut inner = self.inner.write().await;
                if inner.has_session_snapshot {
                    inner.last_received_at = Some(now);
                    inner.is_disconnected = false;
                }
            }

            WireMessage::SecureScreen(payload) => {
                let cleared = !payload.active;
                {
                    let mut inner = self.inner.write().await;
                    inner.last_received_at = Some(now);
                    inner.is_disconnected = false;
                    inner.is_secure_screen_active = payload.active;
                    inner.secure_screen_reason = if payload.active {
                        Some(payload.reason.clone())
                    } else {
                        None
                    };
                    inner.secure_screen_package = payload.active_package.clone();
                }
                let _ = self.secure_tx.send(payload);
                if cleared {
                    self.secure_cleared_notify.notify_waiters();
                }
            }

            WireMessage::VisionFrame(_) => {
                // Vision frames are correlated directly by DaemonTcpClient::request_vision_frame.
            }
        }
    }

    pub async fn mark_disconnected(&self) {
        {
            let mut inner = self.inner.write().await;
            inner.is_disconnected = true;
            inner.has_session_snapshot = false;
            inner.is_secure_screen_active = false;
            inner.secure_screen_reason = None;
        }
        self.snapshot_notify.notify_waiters();
        self.secure_cleared_notify.notify_waiters();
    }

    pub async fn reset(&self) {
        {
            let mut inner = self.inner.write().await;
            inner.current_index = None;
            inner.last_hello = None;
            inner.last_heartbeat = None;
            inner.last_error = None;
            inner.last_received_at = None;
            inner.is_disconnected = true;
            inner.has_session_snapshot = false;
            inner.last_resync_requested_at = None;
            inner.is_secure_screen_active = false;
            inner.secure_screen_reason = None;
            inner.secure_screen_package.clear();
        }
        self.snapshot_notify.notify_waiters();
        self.secure_cleared_notify.notify_waiters();
    }

    pub async fn current_index(&self) -> Option<Arc<ShadowTreeIndex>> {
        self.inner.read().await.current_index.clone()
    }

    pub async fn last_hello(&self) -> Option<WireHelloPayload> {
        self.inner.read().await.last_hello.clone()
    }

    pub async fn has_session_snapshot(&self) -> bool {
        self.inner.read().await.has_session_snapshot
    }

    pub async fn resync_request_count(&self) -> u64 {
        self.inner.read().await.resync_request_count
    }

    pub async fn is_secure_screen_active(&self) -> bool {
        self.inner.read().await.is_secure_screen_active
    }

    pub async fn secure_screen_info(&self) -> (bool, Option<String>, String) {
        let inner = self.inner.read().await;
        (
            inner.is_secure_screen_active,
            inner.secure_screen_reason.clone(),
            inner.secure_screen_package.clone(),
        )
    }

    pub async fn get_current_tree(&self) -> Vec<ShadowUINode> {
        self.inner
            .read()
            .await
            .current_index
            .as_ref()
            .map(|idx| idx.ordered_nodes.clone())
            .unwrap_or_default()
    }

    pub async fn get_compact_tree(&self) -> Vec<ShadowCompactNode> {
        self.inner
            .read()
            .await
            .current_index
            .as_ref()
            .map(|idx| idx.compact_nodes.clone())
            .unwrap_or_default()
    }

    pub async fn get_snapshot_metadata(&self) -> Option<SnapshotMetadata> {
        let inner = self.inner.read().await;
        let idx = inner.current_index.as_ref()?;
        let received = inner.last_received_at?;
        Some(SnapshotMetadata {
            seq: idx.seq,
            package: idx.active_package.clone(),
            window_id: idx.active_window_id,
            age_ms: received.elapsed().as_millis() as u64,
        })
    }

    pub async fn is_fresh(&self, max_age_secs: f64) -> bool {
        let inner = self.inner.read().await;
        if inner.is_disconnected || !inner.has_session_snapshot || inner.current_index.is_none() {
            return false;
        }
        let Some(received) = inner.last_received_at else {
            return false;
        };
        received.elapsed().as_secs_f64() <= max_age_secs
    }

    pub async fn node_for_id(&self, node_id: i64) -> Option<ShadowUINode> {
        self.inner
            .read()
            .await
            .current_index
            .as_ref()
            .and_then(|idx| idx.nodes_by_id.get(&node_id).cloned())
    }

    pub async fn children_of_id(&self, parent_id: i64) -> Vec<ShadowUINode> {
        let inner = self.inner.read().await;
        let Some(idx) = inner.current_index.as_ref() else {
            return Vec::new();
        };
        let Some(child_ids) = idx.children_by_parent_id.get(&parent_id) else {
            return Vec::new();
        };
        child_ids
            .iter()
            .filter_map(|id| idx.nodes_by_id.get(id).cloned())
            .collect()
    }

    pub async fn find_node_by_text(
        &self,
        query: &str,
        mode: ShadowTreeMatchMode,
        prefer_clickable: bool,
    ) -> Option<ShadowUINode> {
        let index = self.current_index().await?;
        if query.is_empty() {
            return None;
        }
        let lowered = query.to_lowercase();

        if let Some(exact_candidates) = index.exact_lowercased_text_nodes(&lowered) {
            if mode == ShadowTreeMatchMode::Exact {
                let case_matched: Vec<&ShadowUINode> = exact_candidates
                    .into_iter()
                    .filter(|n| n.text == query)
                    .collect();
                return Self::best_candidate(&case_matched, prefer_clickable).cloned();
            }
            if let Some(best) = Self::best_candidate(&exact_candidates, prefer_clickable) {
                return Some(best.clone());
            }
        } else if mode == ShadowTreeMatchMode::Exact {
            return None;
        }

        let mut best_node: Option<&ShadowUINode> = None;
        let mut best_score = i32::MIN;
        for node in &index.ordered_nodes {
            if node.text.is_empty() {
                continue;
            }
            let node_lower = node.text.to_lowercase();
            if !node_lower.contains(&lowered) {
                continue;
            }
            let mut score = if node_lower == lowered {
                100
            } else if node_lower.starts_with(&lowered) {
                50
            } else {
                20
            };
            score += Self::common_node_score(node, prefer_clickable);
            if score > best_score {
                best_score = score;
                best_node = Some(node);
            }
        }
        best_node.cloned()
    }

    pub async fn find_node_by_resource_id(
        &self,
        resource_id: &str,
        mode: ShadowTreeMatchMode,
    ) -> Option<ShadowUINode> {
        let index = self.current_index().await?;
        if resource_id.is_empty() {
            return None;
        }

        if let Some(exact) = index.exact_nodes_for_resource_id(resource_id) {
            if let Some(best) = Self::best_candidate(&exact, true) {
                return Some(best.clone());
            }
        }
        if mode == ShadowTreeMatchMode::Exact {
            return None;
        }

        if mode == ShadowTreeMatchMode::SuffixOrExactResourceId {
            if let Some(short_hits) = index.short_id_nodes(resource_id) {
                if let Some(best) = Self::best_candidate(&short_hits, true) {
                    return Some(best.clone());
                }
            }
        }

        let lowered = resource_id.to_lowercase();
        let slash_suffix = format!("/{lowered}");
        let colon_suffix = format!(":{lowered}");
        let mut best_node: Option<&ShadowUINode> = None;
        let mut best_score = i32::MIN;

        for node in &index.ordered_nodes {
            if node.resource_id.is_empty() {
                continue;
            }
            let rid_lower = node.resource_id.to_lowercase();
            let base_score = if rid_lower == lowered {
                Some(120)
            } else if rid_lower.ends_with(&slash_suffix)
                || rid_lower.ends_with(&colon_suffix)
                || rid_lower.ends_with(&lowered)
            {
                Some(80)
            } else if mode == ShadowTreeMatchMode::ContainsCaseInsensitive
                && rid_lower.contains(&lowered)
            {
                Some(30)
            } else {
                None
            };
            if let Some(mut score) = base_score {
                score += Self::common_node_score(node, true);
                if score > best_score {
                    best_score = score;
                    best_node = Some(node);
                }
            }
        }
        best_node.cloned()
    }

    pub async fn find_node_by_content_desc(
        &self,
        query: &str,
        mode: ShadowTreeMatchMode,
    ) -> Option<ShadowUINode> {
        let index = self.current_index().await?;
        if query.is_empty() {
            return None;
        }
        let lowered = query.to_lowercase();

        if let Some(exact_candidates) = index.exact_lowercased_desc_nodes(&lowered) {
            if mode == ShadowTreeMatchMode::Exact {
                let case_matched: Vec<&ShadowUINode> = exact_candidates
                    .into_iter()
                    .filter(|n| n.content_desc == query)
                    .collect();
                return Self::best_candidate(&case_matched, true).cloned();
            }
            if let Some(best) = Self::best_candidate(&exact_candidates, true) {
                return Some(best.clone());
            }
        } else if mode == ShadowTreeMatchMode::Exact {
            return None;
        }

        let mut best_node: Option<&ShadowUINode> = None;
        let mut best_score = i32::MIN;
        for node in &index.ordered_nodes {
            if node.content_desc.is_empty() {
                continue;
            }
            let desc_lower = node.content_desc.to_lowercase();
            if !desc_lower.contains(&lowered) {
                continue;
            }
            let mut score = if desc_lower == lowered {
                100
            } else if desc_lower.starts_with(&lowered) {
                50
            } else {
                20
            };
            score += Self::common_node_score(node, true);
            if score > best_score {
                best_score = score;
                best_node = Some(node);
            }
        }
        best_node.cloned()
    }

    pub async fn wait_for_snapshot(&self, timeout_secs: f64) -> Option<Arc<ShadowTreeIndex>> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.0));
        loop {
            let notified = self.snapshot_notify.notified();
            {
                let inner = self.inner.read().await;
                if inner.has_session_snapshot && !inner.is_disconnected {
                    if let Some(ref idx) = inner.current_index {
                        return Some(Arc::clone(idx));
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return None;
            }
        }
    }

    pub async fn wait_until_secure_screen_cleared(&self, timeout_secs: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs.max(0.0));
        loop {
            let notified = self.secure_cleared_notify.notified();
            {
                let inner = self.inner.read().await;
                if !inner.is_secure_screen_active {
                    return true;
                }
                if inner.is_disconnected {
                    return false;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return false;
            }
        }
    }

    async fn trigger_resync_if_needed(&self, now: Instant) {
        let should_call = {
            let mut inner = self.inner.write().await;
            if let Some(last) = inner.last_resync_requested_at {
                if now.saturating_duration_since(last) < self.resync_cooldown {
                    return;
                }
            }
            inner.last_resync_requested_at = Some(now);
            inner.resync_request_count = inner.resync_request_count.wrapping_add(1);
            true
        };
        if should_call {
            if let Some(ref handler) = self.resync_handler {
                handler().await;
            }
        }
    }

    fn best_candidate<'a>(
        candidates: &[&'a ShadowUINode],
        prefer_clickable: bool,
    ) -> Option<&'a ShadowUINode> {
        candidates
            .iter()
            .copied()
            .max_by_key(|node| Self::common_node_score(node, prefer_clickable))
    }

    fn common_node_score(node: &ShadowUINode, prefer_clickable: bool) -> i32 {
        let mut score = 0;
        if node.visible {
            score += 16;
        }
        if node.enabled {
            score += 8;
        }
        if prefer_clickable && node.clickable {
            score += 4;
        }
        score
    }

    fn apply_patch(patch: &WireNodePatch, node: &mut ShadowUINode) {
        let mask = ShadowPatchMask(patch.mask);
        if let Some(parent_opt) = patch.parent_id_patch {
            node.parent_id = parent_opt;
        }
        if mask.contains(ShadowPatchMask::ORDER) {
            if let Some(v) = patch.index {
                node.index = v;
            }
            if let Some(v) = patch.child_index {
                node.child_index = v;
            }
            if let Some(v) = patch.depth {
                node.depth = v;
            }
        }
        if mask.contains(ShadowPatchMask::TEXT) {
            if let Some(ref v) = patch.text {
                node.text = v.clone();
            }
        }
        if mask.contains(ShadowPatchMask::CONTENT_DESC) {
            if let Some(ref v) = patch.content_desc {
                node.content_desc = v.clone();
            }
        }
        if mask.contains(ShadowPatchMask::RESOURCE_ID) {
            if let Some(ref v) = patch.resource_id {
                node.resource_id = v.clone();
            }
        }
        if mask.contains(ShadowPatchMask::NODE_CLASS) {
            if let Some(ref v) = patch.class_name {
                node.class_name = v.clone();
            }
            if let Some(ref v) = patch.package_name {
                node.package_name = v.clone();
            }
        }
        if mask.contains(ShadowPatchMask::BOUNDS) {
            if let Some(ref v) = patch.bounds {
                node.bounds = v.clone();
            }
            if let Some(v) = patch.center_x {
                node.center_x = v;
            }
            if let Some(v) = patch.center_y {
                node.center_y = v;
            }
        }
        if mask.contains(ShadowPatchMask::FLAGS) {
            if let Some(v) = patch.clickable {
                node.clickable = v;
            }
            if let Some(v) = patch.scrollable {
                node.scrollable = v;
            }
            if let Some(v) = patch.editable {
                node.editable = v;
            }
            if let Some(v) = patch.focused {
                node.focused = v;
            }
            if let Some(v) = patch.checked {
                node.checked = v;
            }
            if let Some(v) = patch.enabled {
                node.enabled = v;
            }
            if let Some(v) = patch.visible {
                node.visible = v;
            }
        }
    }
}

impl Default for ShadowDomState {
    fn default() -> Self {
        Self::new(0.25, None)
    }
}
