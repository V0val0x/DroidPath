use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::shadow_dom::ShadowDOMUpdate;

use super::hasher::{ScreenLayoutHasher, ScreenSemanticFingerprint};
use super::hazard::HazardousActionClassifier;
use super::models::{
    AttributedUIAction, RankedVertexMatch, ScreenStateVertex, ScreenTransitionEdge,
    SpatialGraphSnapshot,
};

pub type TopologyChangedHandler =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RewiredEdgeKey {
    from_hash: u64,
    to_hash: u64,
    action_signature: String,
}

struct GraphInner {
    root_state_hash: Option<u64>,
    current_state_hash: Option<u64>,
    vertices: HashMap<u64, ScreenStateVertex>,
    outgoing_edges: HashMap<u64, Vec<ScreenTransitionEdge>>,
    spanning_children: BTreeMap<u64, Vec<u64>>,
    topology_revision: u64,
    current_fingerprint: Option<ScreenSemanticFingerprint>,
    last_state_transition_at: Option<DateTime<Utc>>,
    pending_action: Option<AttributedUIAction>,
    on_topology_changed: Option<TopologyChangedHandler>,
}

/// Builds a persistent canonical directed multigraph of deduplicated `ScreenStateVertex` nodes
/// while simultaneously maintaining a first-discovery acyclic spanning DAG for deterministic A* planning.
pub struct SpatialGraphEngine {
    hasher: ScreenLayoutHasher,
    significance_threshold: f64,
    pending_action_ttl_secs: f64,
    inner: Mutex<GraphInner>,
}

impl Default for SpatialGraphEngine {
    fn default() -> Self {
        Self::new(ScreenLayoutHasher::default(), 0.72, 5.0)
    }
}

impl SpatialGraphEngine {
    const DEFAULT_CANONICAL_SIMILARITY_THRESHOLD: f64 = 0.90;
    const MINIMUM_CANONICAL_TOKEN_COUNT: usize = 4;

    pub fn new(
        hasher: ScreenLayoutHasher,
        significance_threshold: f64,
        pending_action_ttl_secs: f64,
    ) -> Self {
        Self {
            hasher,
            significance_threshold: significance_threshold.clamp(0.0, 1.0),
            pending_action_ttl_secs: pending_action_ttl_secs.max(0.1),
            inner: Mutex::new(GraphInner {
                root_state_hash: None,
                current_state_hash: None,
                vertices: HashMap::new(),
                outgoing_edges: HashMap::new(),
                spanning_children: BTreeMap::new(),
                topology_revision: 0,
                current_fingerprint: None,
                last_state_transition_at: None,
                pending_action: None,
                on_topology_changed: None,
            }),
        }
    }

    pub async fn set_on_topology_changed(&self, handler: Option<TopologyChangedHandler>) {
        self.inner.lock().await.on_topology_changed = handler;
    }

    pub async fn vertex_count(&self) -> usize {
        self.inner.lock().await.vertices.len()
    }

    pub async fn edge_count(&self) -> usize {
        self.inner
            .lock()
            .await
            .outgoing_edges
            .values()
            .map(Vec::len)
            .sum()
    }

    pub async fn root_state_hash(&self) -> Option<u64> {
        self.inner.lock().await.root_state_hash
    }

    pub async fn current_state_hash(&self) -> Option<u64> {
        self.inner.lock().await.current_state_hash
    }

    pub async fn topology_revision(&self) -> u64 {
        self.inner.lock().await.topology_revision
    }

    pub async fn vertex(&self, hash: u64) -> Option<ScreenStateVertex> {
        self.inner.lock().await.vertices.get(&hash).cloned()
    }

    pub async fn vertices_map(&self) -> HashMap<u64, ScreenStateVertex> {
        self.inner.lock().await.vertices.clone()
    }

    pub async fn record_pending_action(&self, action: AttributedUIAction) {
        self.inner.lock().await.pending_action = Some(action);
    }

    pub async fn process(&self, update: &ShadowDOMUpdate) -> Option<ScreenTransitionEdge> {
        if update.is_secure_screen_active {
            return None;
        }
        let now = Utc::now();
        let fp = self.hasher.fingerprint(&update.index);

        let (result, notify_handler) = {
            let mut g = self.inner.lock().await;
            let mut topology_changed = false;

            // 1. Cold start
            let (Some(prev_hash), Some(prev_fp)) =
                (g.current_state_hash, g.current_fingerprint.clone())
            else {
                if fp.tokens.is_empty() && update.index.ordered_nodes.is_empty() {
                    return None;
                }
                if let Some(exact) = g.vertices.get_mut(&fp.layout_hash) {
                    exact.last_seen_at = now;
                    exact.visit_count += 1;
                    let exact_clone = exact.clone();
                    if g.root_state_hash.is_none() {
                        g.root_state_hash = Some(exact_clone.layout_hash);
                    }
                    g.current_state_hash = Some(exact_clone.layout_hash);
                    g.current_fingerprint =
                        Some(Self::canonical_fingerprint(&exact_clone, fp.active_window_id));
                    g.last_state_transition_at = Some(now);
                    g.pending_action = None;
                    return None;
                }
                if let Some(mut canonical) =
                    Self::best_canonical_match(&g.vertices, &fp, self.significance_threshold)
                {
                    canonical.last_seen_at = now;
                    canonical.visit_count += 1;
                    let c_hash = canonical.layout_hash;
                    g.vertices.insert(c_hash, canonical.clone());
                    if g.root_state_hash.is_none() {
                        g.root_state_hash = Some(c_hash);
                    }
                    g.current_state_hash = Some(c_hash);
                    g.current_fingerprint =
                        Some(Self::canonical_fingerprint(&canonical, fp.active_window_id));
                    g.last_state_transition_at = Some(now);
                    g.pending_action = None;
                    return None;
                }
                let root = ScreenStateVertex {
                    layout_hash: fp.layout_hash,
                    active_package: fp.active_package.clone(),
                    sample_interactive_labels: fp.sample_interactive_labels.clone(),
                    semantic_tokens: fp.tokens.clone(),
                    first_seen_at: now,
                    last_seen_at: now,
                    visit_count: 1,
                    discovery_parent_hash: None,
                    discovery_depth: 0,
                };
                g.vertices.insert(fp.layout_hash, root);
                if g.root_state_hash.is_none() {
                    g.root_state_hash = Some(fp.layout_hash);
                }
                g.current_state_hash = Some(fp.layout_hash);
                g.current_fingerprint = Some(fp);
                g.last_state_transition_at = Some(now);
                g.pending_action = None;
                g.topology_revision = g.topology_revision.wrapping_add(1);
                let handler = g.on_topology_changed.clone();
                drop(g);
                if let Some(h) = handler {
                    tokio::spawn(async move { h().await });
                }
                return None;
            };

            // 2. Identical layout hash
            if fp.layout_hash == prev_hash {
                if let Some(v) = g.vertices.get_mut(&prev_hash) {
                    v.last_seen_at = now;
                }
                g.current_fingerprint = Some(fp);
                return None;
            }

            // 3. Ignore transient empty/status-bar-only transition frames without consuming pendingAction
            if fp.tokens.is_empty() {
                return None;
            }
            if self.hasher.ignored_packages.contains(&fp.active_package)
                && !self
                    .hasher
                    .ignored_packages
                    .contains(&prev_fp.active_package)
                && !update
                    .index
                    .ordered_nodes
                    .iter()
                    .any(|n| n.visible && (n.clickable || n.scrollable || n.editable))
            {
                return None;
            }

            // 4. Known vertex check
            let is_already_known = g.vertices.contains_key(&fp.layout_hash);

            // 5. Same-app minor layout jitter suppression
            if !is_already_known && fp.active_package == prev_fp.active_package {
                let sim = ScreenLayoutHasher::similarity(&prev_fp, &fp);
                if sim >= self.significance_threshold {
                    if let Some(v) = g.vertices.get_mut(&prev_hash) {
                        v.last_seen_at = now;
                    }
                    return None;
                }
            }

            // 6. Cross-app / post-transition canonical deduplication
            let mut resolved_fp = fp.clone();
            if !is_already_known {
                if let Some(canonical) =
                    Self::best_canonical_match(&g.vertices, &fp, self.significance_threshold)
                {
                    if canonical.layout_hash == prev_hash {
                        if let Some(v) = g.vertices.get_mut(&prev_hash) {
                            v.last_seen_at = now;
                        }
                        return None;
                    }
                    resolved_fp = Self::canonical_fingerprint(&canonical, fp.active_window_id);
                }
            }

            let attributed_action =
                Self::consume_valid_pending_action(&mut g, now, self.pending_action_ttl_secs);
            let ref_time = attributed_action
                .as_ref()
                .map(|a| a.timestamp)
                .or(g.last_state_transition_at)
                .unwrap_or(now);
            let transition_ms = (now - ref_time).num_milliseconds().max(0) as f64;

            let is_new_discovery;
            let mut did_upgrade_tokens = false;
            if let Some(existing) = g.vertices.get_mut(&resolved_fp.layout_hash) {
                existing.last_seen_at = now;
                existing.visit_count += 1;
                if existing.semantic_tokens.is_empty() && !resolved_fp.tokens.is_empty() {
                    existing.semantic_tokens = resolved_fp.tokens.clone();
                    did_upgrade_tokens = true;
                }
                is_new_discovery = false;
            } else {
                let parent_depth = g
                    .vertices
                    .get(&prev_hash)
                    .map(|v| v.discovery_depth)
                    .unwrap_or(0);
                let vertex = ScreenStateVertex {
                    layout_hash: resolved_fp.layout_hash,
                    active_package: resolved_fp.active_package.clone(),
                    sample_interactive_labels: resolved_fp.sample_interactive_labels.clone(),
                    semantic_tokens: resolved_fp.tokens.clone(),
                    first_seen_at: now,
                    last_seen_at: now,
                    visit_count: 1,
                    discovery_parent_hash: Some(prev_hash),
                    discovery_depth: parent_depth + 1,
                };
                g.vertices.insert(resolved_fp.layout_hash, vertex);
                g.spanning_children
                    .entry(prev_hash)
                    .or_default()
                    .push(resolved_fp.layout_hash);
                is_new_discovery = true;
            }

            let action_sig = attributed_action
                .as_ref()
                .map(|a| a.signature())
                .unwrap_or_else(|| "implicit".to_string());
            let hazard =
                HazardousActionClassifier::assess(attributed_action.as_ref(), &action_sig, None, None);

            let edges = g.outgoing_edges.entry(prev_hash).or_default();
            let committed_edge;
            let is_new_edge;

            if let Some(existing) = edges
                .iter_mut()
                .find(|e| e.to_hash == resolved_fp.layout_hash && e.action_signature == action_sig)
            {
                let prev_count = existing.traversal_count as f64;
                existing.traversal_count += 1;
                existing.average_transition_ms =
                    ((existing.average_transition_ms * prev_count) + transition_ms)
                        / (existing.traversal_count as f64);
                existing.last_traversed_at = now;
                if hazard.is_hazardous && !existing.is_hazardous {
                    existing.is_hazardous = true;
                    existing.hazard_reason = Some(hazard.reason());
                }
                committed_edge = existing.clone();
                is_new_edge = false;
            } else {
                let edge = ScreenTransitionEdge {
                    id: Uuid::new_v4(),
                    from_hash: prev_hash,
                    to_hash: resolved_fp.layout_hash,
                    action: attributed_action,
                    action_signature: action_sig,
                    traversal_count: 1,
                    average_transition_ms: transition_ms,
                    is_spanning_tree_edge: is_new_discovery,
                    last_traversed_at: now,
                    is_hazardous: hazard.is_hazardous,
                    hazard_reason: hazard.is_hazardous.then(|| hazard.reason()),
                };
                committed_edge = edge.clone();
                edges.push(edge);
                is_new_edge = true;
            }

            g.current_state_hash = Some(resolved_fp.layout_hash);
            g.current_fingerprint = Some(resolved_fp);
            g.last_state_transition_at = Some(now);

            let handler = if is_new_discovery || is_new_edge || did_upgrade_tokens {
                topology_changed = true;
                g.topology_revision = g.topology_revision.wrapping_add(1);
                g.on_topology_changed.clone()
            } else {
                None
            };
            let _ = topology_changed;
            (Some(committed_edge), handler)
        };

        if let Some(h) = notify_handler {
            tokio::spawn(async move { h().await });
        }
        result
    }

    fn canonical_fingerprint(
        vertex: &ScreenStateVertex,
        fallback_active_window_id: i32,
    ) -> ScreenSemanticFingerprint {
        ScreenSemanticFingerprint {
            layout_hash: vertex.layout_hash,
            active_package: vertex.active_package.clone(),
            active_window_id: fallback_active_window_id,
            tokens: vertex.semantic_tokens.clone(),
            sample_interactive_labels: vertex.sample_interactive_labels.clone(),
        }
    }

    fn best_canonical_match(
        vertices: &HashMap<u64, ScreenStateVertex>,
        fp: &ScreenSemanticFingerprint,
        significance_threshold: f64,
    ) -> Option<ScreenStateVertex> {
        if fp.tokens.len() < Self::MINIMUM_CANONICAL_TOKEN_COUNT {
            return None;
        }
        let threshold = Self::DEFAULT_CANONICAL_SIMILARITY_THRESHOLD.max(significance_threshold);
        let mut best_vertex: Option<&ScreenStateVertex> = None;
        let mut best_sim = 0.0_f64;

        for vertex in vertices.values() {
            if vertex.active_package != fp.active_package || vertex.semantic_tokens.is_empty() {
                continue;
            }
            let sim = ScreenLayoutHasher::jaccard_similarity(&vertex.semantic_tokens, &fp.tokens);
            if sim < threshold {
                continue;
            }
            let should_replace = match best_vertex {
                None => true,
                Some(current_best) => {
                    sim > best_sim
                        || (sim == best_sim && vertex.visit_count > current_best.visit_count)
                        || (sim == best_sim
                            && vertex.visit_count == current_best.visit_count
                            && vertex.layout_hash < current_best.layout_hash)
                }
            };
            if should_replace {
                best_vertex = Some(vertex);
                best_sim = sim;
            }
        }
        best_vertex.cloned()
    }

    pub async fn neighbors(&self, state_hash: u64) -> Vec<ScreenTransitionEdge> {
        self.inner
            .lock()
            .await
            .outgoing_edges
            .get(&state_hash)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn predecessors(&self, state_hash: u64) -> Vec<ScreenTransitionEdge> {
        let g = self.inner.lock().await;
        let mut result = Vec::new();
        for edges in g.outgoing_edges.values() {
            for edge in edges {
                if edge.to_hash == state_hash {
                    result.push(edge.clone());
                }
            }
        }
        result
    }

    pub async fn spanning_dag_children(&self, state_hash: u64) -> Vec<u64> {
        self.inner
            .lock()
            .await
            .spanning_children
            .get(&state_hash)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn spanning_path_from_root(&self, state_hash: u64) -> Vec<u64> {
        let g = self.inner.lock().await;
        if !g.vertices.contains_key(&state_hash) {
            return Vec::new();
        }
        let mut reversed_path = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = Some(state_hash);
        while let Some(hash) = cursor {
            if !visited.insert(hash) {
                break;
            }
            reversed_path.push(hash);
            cursor = g.vertices.get(&hash).and_then(|v| v.discovery_parent_hash);
        }
        reversed_path.reverse();
        reversed_path
    }

    pub async fn is_spanning_dag_acyclic(&self) -> bool {
        let g = self.inner.lock().await;
        Self::is_spanning_dag_acyclic_inner(&g.vertices, &g.spanning_children)
    }

    fn is_spanning_dag_acyclic_inner(
        vertices: &HashMap<u64, ScreenStateVertex>,
        spanning_children: &BTreeMap<u64, Vec<u64>>,
    ) -> bool {
        for (&parent, children) in spanning_children {
            for &child in children {
                let Some(child_v) = vertices.get(&child) else {
                    return false;
                };
                if child_v.discovery_parent_hash != Some(parent) {
                    return false;
                }
            }
        }
        for &hash in vertices.keys() {
            let mut seen = HashSet::new();
            let mut cursor = Some(hash);
            while let Some(current) = cursor {
                if !seen.insert(current) {
                    return false;
                }
                cursor = vertices
                    .get(&current)
                    .and_then(|v| v.discovery_parent_hash);
            }
        }
        true
    }

    pub async fn snapshot(&self) -> SpatialGraphSnapshot {
        let g = self.inner.lock().await;
        let mut sorted_vertices: Vec<ScreenStateVertex> = g.vertices.values().cloned().collect();
        sorted_vertices.sort_by(|a, b| {
            a.first_seen_at
                .cmp(&b.first_seen_at)
                .then_with(|| a.layout_hash.cmp(&b.layout_hash))
        });

        let mut all_edges: Vec<ScreenTransitionEdge> = g
            .outgoing_edges
            .values()
            .flat_map(|es| es.iter().cloned())
            .collect();
        all_edges.sort_by(|a, b| {
            a.last_traversed_at
                .cmp(&b.last_traversed_at)
                .then_with(|| a.id.cmp(&b.id))
        });

        SpatialGraphSnapshot {
            root_state_hash: g.root_state_hash,
            current_state_hash: g.current_state_hash,
            vertices: sorted_vertices,
            edges: all_edges,
            spanning_children: g.spanning_children.clone(),
        }
    }

    pub async fn export_graph_json(&self, pretty_printed: bool) -> Result<Vec<u8>, serde_json::Error> {
        let snap = self.snapshot().await;
        if pretty_printed {
            serde_json::to_vec_pretty(&snap)
        } else {
            serde_json::to_vec(&snap)
        }
    }

    pub async fn import_graph_json(&self, data: &[u8]) -> Result<(), serde_json::Error> {
        let snap: SpatialGraphSnapshot = serde_json::from_slice(data)?;
        self.import_snapshot(snap).await;
        Ok(())
    }

    pub async fn import_snapshot(&self, snapshot: SpatialGraphSnapshot) {
        let mut g = self.inner.lock().await;
        Self::reset_inner(&mut g);

        // 1. Canonicalize persisted tokens and deduplicate exact layoutHash collisions
        let mut raw_vertices_by_hash: HashMap<u64, ScreenStateVertex> =
            HashMap::with_capacity(snapshot.vertices.len());
        for mut vertex in snapshot.vertices {
            if !vertex.semantic_tokens.is_empty() {
                let canonicalized: BTreeSet<String> = vertex
                    .semantic_tokens
                    .iter()
                    .map(|t| ScreenLayoutHasher::canonicalize_token(t, &vertex.active_package))
                    .collect();
                let pruned: BTreeSet<String> = canonicalized
                    .iter()
                    .filter(|t| !ScreenLayoutHasher::is_unanchored_image_token(t))
                    .cloned()
                    .collect();
                vertex.semantic_tokens = if pruned.len() >= Self::MINIMUM_CANONICAL_TOKEN_COUNT {
                    pruned
                } else {
                    canonicalized
                };
            }
            if let Some(existing) = raw_vertices_by_hash.get(&vertex.layout_hash).cloned() {
                let earliest_first = existing.first_seen_at.min(vertex.first_seen_at);
                let latest_last = existing.last_seen_at.max(vertex.last_seen_at);
                let merged_labels = if existing.sample_interactive_labels.is_empty() {
                    vertex.sample_interactive_labels
                } else {
                    existing.sample_interactive_labels
                };
                let merged_tokens = if existing.semantic_tokens.is_empty() {
                    vertex.semantic_tokens
                } else {
                    existing.semantic_tokens
                };
                let keep_existing_discovery = existing.first_seen_at <= vertex.first_seen_at;
                raw_vertices_by_hash.insert(
                    existing.layout_hash,
                    ScreenStateVertex {
                        layout_hash: existing.layout_hash,
                        active_package: existing.active_package,
                        sample_interactive_labels: merged_labels,
                        semantic_tokens: merged_tokens,
                        first_seen_at: earliest_first,
                        last_seen_at: latest_last,
                        visit_count: existing.visit_count + vertex.visit_count.max(0),
                        discovery_parent_hash: if keep_existing_discovery {
                            existing.discovery_parent_hash
                        } else {
                            vertex.discovery_parent_hash
                        },
                        discovery_depth: if keep_existing_discovery {
                            existing.discovery_depth
                        } else {
                            vertex.discovery_depth
                        },
                    },
                );
            } else {
                raw_vertices_by_hash.insert(vertex.layout_hash, vertex);
            }
        }

        // 2. Sort deterministically by canonical priority and compact duplicate vertices (Jaccard >= threshold)
        let threshold =
            Self::DEFAULT_CANONICAL_SIMILARITY_THRESHOLD.max(self.significance_threshold);
        let mut sorted_vertices: Vec<ScreenStateVertex> =
            raw_vertices_by_hash.into_values().collect();
        sorted_vertices.sort_by(|lhs, rhs| {
            let lhs_is_root = Some(lhs.layout_hash) == snapshot.root_state_hash;
            let rhs_is_root = Some(rhs.layout_hash) == snapshot.root_state_hash;
            rhs_is_root
                .cmp(&lhs_is_root)
                .then_with(|| rhs.visit_count.cmp(&lhs.visit_count))
                .then_with(|| lhs.first_seen_at.cmp(&rhs.first_seen_at))
                .then_with(|| rhs.semantic_tokens.len().cmp(&lhs.semantic_tokens.len()))
                .then_with(|| lhs.layout_hash.cmp(&rhs.layout_hash))
        });

        let mut canonical_map: HashMap<u64, u64> = HashMap::with_capacity(sorted_vertices.len());
        let mut canonical_vertices: HashMap<u64, ScreenStateVertex> =
            HashMap::with_capacity(sorted_vertices.len());
        let mut leaders_by_package: HashMap<String, Vec<u64>> = HashMap::new();
        let mut merged_vertex_count = 0usize;

        for vertex in sorted_vertices {
            let mut matched_leader_hash: Option<u64> = None;
            let mut best_similarity = 0.0_f64;

            if !vertex.semantic_tokens.is_empty() {
                if let Some(package_leaders) = leaders_by_package.get(&vertex.active_package) {
                    for &leader_hash in package_leaders {
                        let Some(leader) = canonical_vertices.get(&leader_hash) else {
                            continue;
                        };
                        if leader.semantic_tokens.is_empty() {
                            continue;
                        }
                        let sim = ScreenLayoutHasher::jaccard_similarity(
                            &leader.semantic_tokens,
                            &vertex.semantic_tokens,
                        );
                        let is_eligible = (leader.semantic_tokens.len().min(vertex.semantic_tokens.len())
                            >= Self::MINIMUM_CANONICAL_TOKEN_COUNT
                            && sim >= threshold)
                            || sim == 1.0;
                        if !is_eligible {
                            continue;
                        }
                        if matched_leader_hash.is_none() || sim > best_similarity {
                            matched_leader_hash = Some(leader_hash);
                            best_similarity = sim;
                        }
                    }
                }
            }

            if let Some(leader_hash) = matched_leader_hash {
                if let Some(leader) = canonical_vertices.get_mut(&leader_hash) {
                    canonical_map.insert(vertex.layout_hash, leader_hash);
                    merged_vertex_count += 1;
                    if leader.sample_interactive_labels.is_empty() {
                        leader.sample_interactive_labels = vertex.sample_interactive_labels;
                    }
                    leader.first_seen_at = leader.first_seen_at.min(vertex.first_seen_at);
                    leader.last_seen_at = leader.last_seen_at.max(vertex.last_seen_at);
                    leader.visit_count += vertex.visit_count.max(0);
                }
            } else {
                let hash = vertex.layout_hash;
                let pkg = vertex.active_package.clone();
                canonical_map.insert(hash, hash);
                canonical_vertices.insert(hash, vertex);
                leaders_by_package.entry(pkg).or_default().push(hash);
            }
        }

        g.vertices = canonical_vertices;

        g.root_state_hash = snapshot.root_state_hash.and_then(|raw| {
            let mapped = canonical_map.get(&raw).copied().unwrap_or(raw);
            g.vertices.contains_key(&mapped).then_some(mapped)
        });

        g.current_state_hash = snapshot.current_state_hash.and_then(|raw| {
            let mapped = canonical_map.get(&raw).copied().unwrap_or(raw);
            g.vertices.contains_key(&mapped).then_some(mapped)
        });

        // 3. Rewire valid edges onto canonical vertices
        let mut deduplicated_edges: HashMap<RewiredEdgeKey, ScreenTransitionEdge> = HashMap::new();
        let mut ordered_edge_keys: Vec<RewiredEdgeKey> = Vec::new();
        let mut dropped_or_merged_edge_count = 0usize;

        for edge in snapshot.edges {
            let (Some(&mapped_from), Some(&mapped_to)) =
                (canonical_map.get(&edge.from_hash), canonical_map.get(&edge.to_hash))
            else {
                dropped_or_merged_edge_count += 1;
                continue;
            };
            if !g.vertices.contains_key(&mapped_from) || !g.vertices.contains_key(&mapped_to) {
                dropped_or_merged_edge_count += 1;
                continue;
            }
            if mapped_from == mapped_to {
                dropped_or_merged_edge_count += 1;
                continue;
            }

            let was_endpoint_merged = (mapped_from != edge.from_hash) || (mapped_to != edge.to_hash);
            if edge.action.is_none() && was_endpoint_merged {
                dropped_or_merged_edge_count += 1;
                continue;
            }

            let key = RewiredEdgeKey {
                from_hash: mapped_from,
                to_hash: mapped_to,
                action_signature: edge.action_signature.clone(),
            };

            if let Some(existing) = deduplicated_edges.get(&key).cloned() {
                dropped_or_merged_edge_count += 1;
                let existing_count = existing.traversal_count.max(0);
                let incoming_count = edge.traversal_count.max(0);
                let total_count = existing_count + incoming_count;
                let merged_avg_ms = if total_count > 0 {
                    ((existing.average_transition_ms * existing_count as f64)
                        + (edge.average_transition_ms * incoming_count as f64))
                        / (total_count as f64)
                } else {
                    existing.average_transition_ms
                };
                let newer_action = if edge.last_traversed_at >= existing.last_traversed_at {
                    edge.action.or(existing.action)
                } else {
                    existing.action.or(edge.action)
                };
                let hazard = HazardousActionClassifier::assess(
                    newer_action.as_ref(),
                    &edge.action_signature,
                    None,
                    None,
                );
                let merged_hazardous =
                    existing.is_hazardous || edge.is_hazardous || hazard.is_hazardous;
                let merged_reason = existing
                    .hazard_reason
                    .or(edge.hazard_reason)
                    .or_else(|| hazard.is_hazardous.then(|| hazard.reason()));
                deduplicated_edges.insert(
                    key,
                    ScreenTransitionEdge {
                        id: existing.id,
                        from_hash: mapped_from,
                        to_hash: mapped_to,
                        action: newer_action,
                        action_signature: edge.action_signature,
                        traversal_count: total_count.max(1),
                        average_transition_ms: merged_avg_ms,
                        is_spanning_tree_edge: existing.is_spanning_tree_edge
                            || edge.is_spanning_tree_edge,
                        last_traversed_at: existing.last_traversed_at.max(edge.last_traversed_at),
                        is_hazardous: merged_hazardous,
                        hazard_reason: if merged_hazardous { merged_reason } else { None },
                    },
                );
            } else {
                let hazard = HazardousActionClassifier::assess(
                    edge.action.as_ref(),
                    &edge.action_signature,
                    None,
                    None,
                );
                let is_haz = edge.is_hazardous || hazard.is_hazardous;
                let haz_reason = edge
                    .hazard_reason
                    .or_else(|| hazard.is_hazardous.then(|| hazard.reason()));
                deduplicated_edges.insert(
                    key.clone(),
                    ScreenTransitionEdge {
                        id: edge.id,
                        from_hash: mapped_from,
                        to_hash: mapped_to,
                        action: edge.action,
                        action_signature: edge.action_signature,
                        traversal_count: edge.traversal_count,
                        average_transition_ms: edge.average_transition_ms,
                        is_spanning_tree_edge: edge.is_spanning_tree_edge,
                        last_traversed_at: edge.last_traversed_at,
                        is_hazardous: is_haz,
                        hazard_reason: if is_haz { haz_reason } else { None },
                    },
                );
                ordered_edge_keys.push(key);
            }
        }

        for key in ordered_edge_keys {
            if let Some(edge) = deduplicated_edges.remove(&key) {
                g.outgoing_edges
                    .entry(edge.from_hash)
                    .or_default()
                    .push(edge);
            }
        }

        // 4. Preserve or rebuild acyclic spanning DAG invariants
        let filtered_spanning: BTreeMap<u64, Vec<u64>> = snapshot
            .spanning_children
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if merged_vertex_count == 0
            && dropped_or_merged_edge_count == 0
            && Self::has_valid_spanning_tree(&g.vertices, &g.outgoing_edges, &filtered_spanning)
        {
            g.spanning_children = filtered_spanning;
        } else {
            Self::rebuild_spanning_dag(&mut g);
        }

        if let Some(cur_hash) = g.current_state_hash {
            if let Some(cur_vertex) = g.vertices.get(&cur_hash) {
                g.current_fingerprint = Some(ScreenSemanticFingerprint {
                    layout_hash: cur_vertex.layout_hash,
                    active_package: cur_vertex.active_package.clone(),
                    active_window_id: 0,
                    tokens: cur_vertex.semantic_tokens.clone(),
                    sample_interactive_labels: cur_vertex.sample_interactive_labels.clone(),
                });
            }
        }
    }

    fn has_valid_spanning_tree(
        vertices: &HashMap<u64, ScreenStateVertex>,
        outgoing_edges: &HashMap<u64, Vec<ScreenTransitionEdge>>,
        candidate_spanning_children: &BTreeMap<u64, Vec<u64>>,
    ) -> bool {
        let mut expected_children: HashMap<u64, HashSet<u64>> = HashMap::new();
        for (&hash, vertex) in vertices {
            if let Some(parent) = vertex.discovery_parent_hash {
                if parent == hash {
                    return false;
                }
                let Some(parent_v) = vertices.get(&parent) else {
                    return false;
                };
                if vertex.discovery_depth != parent_v.discovery_depth + 1 {
                    return false;
                }
                let has_edge = outgoing_edges
                    .get(&parent)
                    .map(|es| {
                        es.iter()
                            .any(|e| e.to_hash == hash && e.is_spanning_tree_edge)
                    })
                    .unwrap_or(false);
                if !has_edge {
                    return false;
                }
                expected_children.entry(parent).or_default().insert(hash);
            } else if vertex.discovery_depth != 0 {
                return false;
            }
        }

        for (&from_hash, edges) in outgoing_edges {
            for edge in edges {
                if edge.is_spanning_tree_edge {
                    let ok = expected_children
                        .get(&from_hash)
                        .map(|s| s.contains(&edge.to_hash))
                        .unwrap_or(false);
                    if !ok {
                        return false;
                    }
                }
            }
        }

        for (&parent, children) in candidate_spanning_children {
            if !vertices.contains_key(&parent) {
                return false;
            }
            let unique: HashSet<u64> = children.iter().copied().collect();
            if unique.len() != children.len() {
                return false;
            }
            for &child in children {
                if vertices.get(&child).and_then(|v| v.discovery_parent_hash) != Some(parent) {
                    return false;
                }
            }
            if !children.is_empty() && expected_children.get(&parent) != Some(&unique) {
                return false;
            }
        }

        for (parent, child_set) in &expected_children {
            if !child_set.is_empty() {
                let actual: HashSet<u64> = candidate_spanning_children
                    .get(parent)
                    .map(|v| v.iter().copied().collect())
                    .unwrap_or_default();
                if &actual != child_set {
                    return false;
                }
            }
        }

        Self::is_spanning_dag_acyclic_inner(vertices, candidate_spanning_children)
    }

    fn rebuild_spanning_dag(g: &mut GraphInner) {
        let mut new_spanning_children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut visited: HashSet<u64> = HashSet::with_capacity(g.vertices.len());
        let mut spanning_edge_ids: HashSet<Uuid> = HashSet::new();

        let mut sorted_outgoing: HashMap<u64, Vec<ScreenTransitionEdge>> =
            HashMap::with_capacity(g.outgoing_edges.len());
        for (&from_hash, edges) in &g.outgoing_edges {
            let mut sorted = edges.clone();
            sorted.sort_by(|lhs, rhs| {
                let lhs_exec = lhs.action.is_some();
                let rhs_exec = rhs.action.is_some();
                rhs_exec
                    .cmp(&lhs_exec)
                    .then_with(|| rhs.is_spanning_tree_edge.cmp(&lhs.is_spanning_tree_edge))
                    .then_with(|| rhs.traversal_count.cmp(&lhs.traversal_count))
                    .then_with(|| lhs.action_signature.cmp(&rhs.action_signature))
                    .then_with(|| lhs.to_hash.cmp(&rhs.to_hash))
                    .then_with(|| lhs.id.cmp(&rhs.id))
            });
            sorted_outgoing.insert(from_hash, sorted);
        }

        let bfs_from_component_root = |start_hash: u64,
                                       vertices: &mut HashMap<u64, ScreenStateVertex>,
                                       visited: &mut HashSet<u64>,
                                       new_spanning_children: &mut BTreeMap<u64, Vec<u64>>,
                                       spanning_edge_ids: &mut HashSet<Uuid>| {
            if !vertices.contains_key(&start_hash) || !visited.insert(start_hash) {
                return;
            }
            if let Some(sv) = vertices.get_mut(&start_hash) {
                sv.discovery_parent_hash = None;
                sv.discovery_depth = 0;
            }

            let mut component_visited_order = vec![start_hash];
            let mut executable_queue = VecDeque::new();
            executable_queue.push_back(start_hash);

            let visit_child = |v: u64,
                               u: u64,
                               edge_id: Uuid,
                               vertices: &mut HashMap<u64, ScreenStateVertex>,
                               new_spanning_children: &mut BTreeMap<u64, Vec<u64>>,
                               spanning_edge_ids: &mut HashSet<Uuid>,
                               component_visited_order: &mut Vec<u64>,
                               executable_queue: &mut VecDeque<u64>| {
                let u_depth = vertices.get(&u).map(|x| x.discovery_depth).unwrap_or(0);
                if let Some(vv) = vertices.get_mut(&v) {
                    vv.discovery_parent_hash = Some(u);
                    vv.discovery_depth = u_depth + 1;
                    new_spanning_children.entry(u).or_default().push(v);
                    spanning_edge_ids.insert(edge_id);
                    component_visited_order.push(v);
                    executable_queue.push_back(v);
                }
            };

            let drain_executable_bfs = |vertices: &mut HashMap<u64, ScreenStateVertex>,
                                        visited: &mut HashSet<u64>,
                                        new_spanning_children: &mut BTreeMap<u64, Vec<u64>>,
                                        spanning_edge_ids: &mut HashSet<Uuid>,
                                        component_visited_order: &mut Vec<u64>,
                                        executable_queue: &mut VecDeque<u64>| {
                while let Some(u) = executable_queue.pop_front() {
                    if let Some(edges) = sorted_outgoing.get(&u) {
                        for edge in edges {
                            if edge.action.is_none() {
                                continue;
                            }
                            let v = edge.to_hash;
                            if v == u || !vertices.contains_key(&v) || !visited.insert(v) {
                                continue;
                            }
                            visit_child(
                                v,
                                u,
                                edge.id,
                                vertices,
                                new_spanning_children,
                                spanning_edge_ids,
                                component_visited_order,
                                executable_queue,
                            );
                        }
                    }
                }
            };

            drain_executable_bfs(
                vertices,
                visited,
                new_spanning_children,
                spanning_edge_ids,
                &mut component_visited_order,
                &mut executable_queue,
            );

            let mut implicit_scan_index = 0usize;
            while implicit_scan_index < component_visited_order.len() {
                let u = component_visited_order[implicit_scan_index];
                implicit_scan_index += 1;
                if let Some(edges) = sorted_outgoing.get(&u) {
                    for edge in edges {
                        if edge.action.is_some() {
                            continue;
                        }
                        let v = edge.to_hash;
                        if v == u || !vertices.contains_key(&v) || !visited.insert(v) {
                            continue;
                        }
                        visit_child(
                            v,
                            u,
                            edge.id,
                            vertices,
                            new_spanning_children,
                            spanning_edge_ids,
                            &mut component_visited_order,
                            &mut executable_queue,
                        );
                        drain_executable_bfs(
                            vertices,
                            visited,
                            new_spanning_children,
                            spanning_edge_ids,
                            &mut component_visited_order,
                            &mut executable_queue,
                        );
                    }
                }
            }
        };

        if let Some(root) = g.root_state_hash {
            if g.vertices.contains_key(&root) {
                bfs_from_component_root(
                    root,
                    &mut g.vertices,
                    &mut visited,
                    &mut new_spanning_children,
                    &mut spanning_edge_ids,
                );
            }
        }

        while visited.len() < g.vertices.len() {
            let mut remaining_in_degree: HashMap<u64, usize> = HashMap::new();
            for &hash in g.vertices.keys() {
                if !visited.contains(&hash) {
                    remaining_in_degree.insert(hash, 0);
                }
            }
            for (&from_hash, edges) in &sorted_outgoing {
                if visited.contains(&from_hash) {
                    continue;
                }
                for edge in edges {
                    if edge.to_hash != from_hash && !visited.contains(&edge.to_hash) {
                        *remaining_in_degree.entry(edge.to_hash).or_insert(0) += 1;
                    }
                }
            }

            let next_root = remaining_in_degree
                .keys()
                .copied()
                .min_by(|&lhs, &rhs| {
                    let lhs_zero = remaining_in_degree.get(&lhs).copied().unwrap_or(0) == 0;
                    let rhs_zero = remaining_in_degree.get(&rhs).copied().unwrap_or(0) == 0;
                    rhs_zero
                        .cmp(&lhs_zero)
                        .then_with(|| {
                            let lv = &g.vertices[&lhs];
                            let rv = &g.vertices[&rhs];
                            lv.first_seen_at.cmp(&rv.first_seen_at)
                        })
                        .then_with(|| lhs.cmp(&rhs))
                });

            let Some(next_root) = next_root else {
                break;
            };
            bfs_from_component_root(
                next_root,
                &mut g.vertices,
                &mut visited,
                &mut new_spanning_children,
                &mut spanning_edge_ids,
            );
        }

        g.spanning_children = new_spanning_children;

        for edges in g.outgoing_edges.values_mut() {
            for edge in edges.iter_mut() {
                edge.is_spanning_tree_edge = spanning_edge_ids.contains(&edge.id);
            }
        }
    }

    pub async fn resolve_target_vertices(
        &self,
        query: &str,
        limit: usize,
    ) -> Vec<RankedVertexMatch> {
        let g = self.inner.lock().await;
        let edges: Vec<&ScreenTransitionEdge> = g
            .outgoing_edges
            .values()
            .flat_map(|es| es.iter())
            .collect();
        Self::resolve_target_vertices_in(query, g.vertices.values(), edges, limit)
    }

    pub fn resolve_target_vertices_in<'a, V, E>(
        query: &str,
        vertices: V,
        edges: E,
        limit: usize,
    ) -> Vec<RankedVertexMatch>
    where
        V: IntoIterator<Item = &'a ScreenStateVertex>,
        E: IntoIterator<Item = &'a ScreenTransitionEdge>,
    {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        let q = trimmed.to_lowercase();
        let normalized_q = ScreenLayoutHasher::normalize_label(trimmed).to_lowercase();

        let mut incoming_score_boost: HashMap<u64, i32> = HashMap::new();
        for edge in edges {
            let Some(ref action) = edge.action else {
                continue;
            };
            let candidates = [
                action.target_description.as_str(),
                action.selector_text.as_deref().unwrap_or(""),
                action.selector_resource_id.as_deref().unwrap_or(""),
                action.selector_content_desc.as_deref().unwrap_or(""),
            ];
            let mut boost = 0;
            for raw in candidates {
                if raw.is_empty() {
                    continue;
                }
                let lower = raw.to_lowercase();
                if lower == q {
                    boost = boost.max(350);
                } else if lower.starts_with(&q) {
                    boost = boost.max(200);
                } else if lower.contains(&q) {
                    boost = boost.max(100);
                }
            }
            if boost > 0 {
                let entry = incoming_score_boost.entry(edge.to_hash).or_insert(0);
                if boost > *entry {
                    *entry = boost;
                }
            }
        }

        let mut matches = Vec::new();
        for vertex in vertices {
            let mut base_score = 0;

            let pkg = vertex.active_package.to_lowercase();
            if pkg == q {
                base_score = base_score.max(1000);
            } else if pkg.starts_with(&q) {
                base_score = base_score.max(500);
            } else if pkg.contains(&q) {
                base_score = base_score.max(220);
            }

            for (idx, label) in vertex.sample_interactive_labels.iter().enumerate() {
                if label.is_empty() {
                    continue;
                }
                let lower = label.to_lowercase();
                let first_item_bonus = if idx == 0 { 20 } else { 0 };
                if lower == q {
                    base_score = base_score.max(780 + first_item_bonus);
                } else if lower.starts_with(&q) {
                    base_score = base_score.max(440 + first_item_bonus);
                } else if lower.contains(&q) {
                    base_score = base_score.max(190 + first_item_bonus);
                }
            }

            for token in &vertex.semantic_tokens {
                let mut parts = token.splitn(3, '|');
                let class_name = parts.next().unwrap_or("");
                let res_id = parts.next().unwrap_or("").to_lowercase();
                let tok_label = parts.next().unwrap_or("").to_lowercase();
                let is_textview_title =
                    (class_name == "TextView" || class_name.ends_with(".TextView"))
                        && res_id.is_empty();
                let title_bonus = if is_textview_title { 60 } else { 0 };

                if !tok_label.is_empty() {
                    if tok_label == q || (!normalized_q.is_empty() && tok_label == normalized_q) {
                        base_score = base_score.max(800 + title_bonus);
                    } else if tok_label.starts_with(&q)
                        || (!normalized_q.is_empty() && tok_label.starts_with(&normalized_q))
                    {
                        base_score = base_score.max(420);
                    } else if tok_label.contains(&q)
                        || (!normalized_q.is_empty() && tok_label.contains(&normalized_q))
                    {
                        base_score = base_score.max(160);
                    }
                }

                if !res_id.is_empty() {
                    let short_id = if let Some(idx) = res_id.find(":id/") {
                        &res_id[idx + 4..]
                    } else if let Some(slash) = res_id.rfind('/') {
                        &res_id[slash + 1..]
                    } else {
                        res_id.as_str()
                    };
                    if res_id == q || short_id == q {
                        base_score = base_score.max(820);
                    } else if short_id.starts_with(&q) || res_id.starts_with(&q) {
                        base_score = base_score.max(420);
                    } else if res_id.contains(&q) {
                        base_score = base_score.max(180);
                    }
                }
            }

            let edge_boost = incoming_score_boost
                .get(&vertex.layout_hash)
                .copied()
                .unwrap_or(0);
            let total_score = if base_score > 0 {
                base_score + edge_boost
            } else if edge_boost >= 350 {
                600
            } else {
                edge_boost
            };

            if total_score > 0 {
                matches.push(RankedVertexMatch {
                    vertex: vertex.clone(),
                    score: total_score,
                });
            }
        }

        matches.sort_by(|lhs, rhs| {
            rhs.score
                .cmp(&lhs.score)
                .then_with(|| rhs.vertex.visit_count.cmp(&lhs.vertex.visit_count))
                .then_with(|| rhs.vertex.last_seen_at.cmp(&lhs.vertex.last_seen_at))
                .then_with(|| lhs.vertex.layout_hash.cmp(&rhs.vertex.layout_hash))
        });

        matches.truncate(limit.max(1));
        matches
    }

    pub async fn reset(&self) {
        let mut g = self.inner.lock().await;
        Self::reset_inner(&mut g);
    }

    fn reset_inner(g: &mut GraphInner) {
        g.root_state_hash = None;
        g.current_state_hash = None;
        g.current_fingerprint = None;
        g.last_state_transition_at = None;
        g.pending_action = None;
        g.vertices.clear();
        g.outgoing_edges.clear();
        g.spanning_children.clear();
    }

    fn consume_valid_pending_action(
        g: &mut GraphInner,
        now: DateTime<Utc>,
        ttl_secs: f64,
    ) -> Option<AttributedUIAction> {
        let pending = g.pending_action.take()?;
        let elapsed_secs = (now - pending.timestamp).num_milliseconds() as f64 / 1000.0;
        if elapsed_secs <= ttl_secs {
            Some(pending)
        } else {
            None
        }
    }
}
