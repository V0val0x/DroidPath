use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

use crate::spatial_graph::{
    ScreenLayoutHasher, ScreenStateVertex, ScreenTransitionEdge, SpatialGraphSnapshot,
};

#[derive(Debug, Clone, PartialEq)]
pub struct AStarCostConfiguration {
    pub min_step_cost: f64,
    pub latency_weight_per_second: f64,
    pub max_steps: usize,
    pub max_node_expansions: usize,
}

impl Default for AStarCostConfiguration {
    fn default() -> Self {
        Self {
            min_step_cost: 1.0,
            latency_weight_per_second: 0.05,
            max_steps: 12,
            max_node_expansions: 2048,
        }
    }
}

impl AStarCostConfiguration {
    pub fn new(
        min_step_cost: f64,
        latency_weight_per_second: f64,
        max_steps: usize,
        max_node_expansions: usize,
    ) -> Self {
        Self {
            min_step_cost: min_step_cost.max(0.01),
            latency_weight_per_second: latency_weight_per_second.max(0.0),
            max_steps: max_steps.max(1),
            max_node_expansions: max_node_expansions.max(1),
        }
    }

    pub fn with_max_steps(max_steps: usize) -> Self {
        Self {
            max_steps: max_steps.max(1),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AStarSearchResult {
    pub start_hash: u64,
    pub target_hash: u64,
    pub edges: Vec<ScreenTransitionEdge>,
    pub total_cost: f64,
    pub expanded_node_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AStarStateKey {
    vertex_hash: u64,
    hop_count: usize,
}

#[derive(Debug, Clone, Copy)]
struct AStarOpenNode {
    vertex_hash: u64,
    g_score: f64,
    h_score: f64,
    f_score: f64,
    hop_count: usize,
}

impl PartialEq for AStarOpenNode {
    fn eq(&self, other: &Self) -> bool {
        self.vertex_hash == other.vertex_hash
            && self.hop_count == other.hop_count
            && self.f_score.to_bits() == other.f_score.to_bits()
            && self.h_score.to_bits() == other.h_score.to_bits()
    }
}

impl Eq for AStarOpenNode {}

/// Reverse ordering for `std::collections::BinaryHeap` so it pops the minimum node
/// matching Swift's `AStarOpenNode.precedes`.
impl Ord for AStarOpenNode {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .f_score
            .total_cmp(&self.f_score)
            .then_with(|| other.h_score.total_cmp(&self.h_score))
            .then_with(|| other.hop_count.cmp(&self.hop_count))
            .then_with(|| other.vertex_hash.cmp(&self.vertex_hash))
    }
}

impl PartialOrd for AStarOpenNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Deterministic A* pathfinder over `SpatialGraphSnapshot` using a binary min-heap priority queue
/// and an admissible, consistent Jaccard token-similarity heuristic.
#[derive(Debug, Clone, Default)]
pub struct AStarPathfinder {
    pub configuration: AStarCostConfiguration,
}

impl AStarPathfinder {
    pub fn new(configuration: AStarCostConfiguration) -> Self {
        Self { configuration }
    }

    pub fn find_shortest_path(
        &self,
        start_hash: u64,
        target_hash: u64,
        graph: &SpatialGraphSnapshot,
    ) -> Option<AStarSearchResult> {
        let mut goals = HashSet::new();
        goals.insert(target_hash);
        self.find_shortest_path_to_any(start_hash, &goals, graph)
    }

    pub fn find_shortest_path_to_any(
        &self,
        start_hash: u64,
        target_hashes: &HashSet<u64>,
        graph: &SpatialGraphSnapshot,
    ) -> Option<AStarSearchResult> {
        self.search(start_hash, target_hashes, graph, false)
    }

    pub fn find_spanning_dag_path(
        &self,
        start_hash: u64,
        target_hash: u64,
        graph: &SpatialGraphSnapshot,
    ) -> Option<AStarSearchResult> {
        let mut goals = HashSet::new();
        goals.insert(target_hash);
        self.search(start_hash, &goals, graph, true)
    }

    pub fn reachable_executable_vertex_count(
        &self,
        start_hash: u64,
        graph: &SpatialGraphSnapshot,
    ) -> usize {
        let mut outgoing: HashMap<u64, Vec<u64>> = HashMap::new();
        for edge in &graph.edges {
            if edge.action.is_none() || edge.to_hash == edge.from_hash {
                continue;
            }
            outgoing.entry(edge.from_hash).or_default().push(edge.to_hash);
        }
        let mut visited: HashSet<u64> = HashSet::new();
        visited.insert(start_hash);
        let mut queue = VecDeque::new();
        queue.push_back(start_hash);
        while let Some(current) = queue.pop_front() {
            if let Some(nexts) = outgoing.get(&current) {
                for &next in nexts {
                    if visited.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        visited.len()
    }

    fn search(
        &self,
        start_hash: u64,
        target_hashes: &HashSet<u64>,
        graph: &SpatialGraphSnapshot,
        spanning_tree_only: bool,
    ) -> Option<AStarSearchResult> {
        if target_hashes.is_empty() {
            return None;
        }

        let mut vertices_by_hash: HashMap<u64, &ScreenStateVertex> =
            HashMap::with_capacity(graph.vertices.len());
        for vertex in &graph.vertices {
            vertices_by_hash.insert(vertex.layout_hash, vertex);
        }
        vertices_by_hash.get(&start_hash)?;

        let valid_targets: HashSet<u64> = target_hashes
            .iter()
            .copied()
            .filter(|h| vertices_by_hash.contains_key(h))
            .collect();
        if valid_targets.is_empty() {
            return None;
        }

        if valid_targets.contains(&start_hash) {
            return Some(AStarSearchResult {
                start_hash,
                target_hash: start_hash,
                edges: Vec::new(),
                total_cost: 0.0,
                expanded_node_count: 0,
            });
        }

        let mut outgoing: HashMap<u64, Vec<&ScreenTransitionEdge>> = HashMap::new();
        for edge in &graph.edges {
            if edge.action.is_none() || edge.to_hash == edge.from_hash {
                continue;
            }
            if spanning_tree_only && !edge.is_spanning_tree_edge {
                continue;
            }
            outgoing.entry(edge.from_hash).or_default().push(edge);
        }
        for edges in outgoing.values_mut() {
            edges.sort_by(|lhs, rhs| {
                lhs.action_signature
                    .cmp(&rhs.action_signature)
                    .then_with(|| lhs.to_hash.cmp(&rhs.to_hash))
                    .then_with(|| lhs.id.cmp(&rhs.id))
            });
        }

        let mut sorted_targets: Vec<u64> = valid_targets.iter().copied().collect();
        sorted_targets.sort_unstable();
        let goal_descriptors: Vec<(String, BTreeSet<String>)> = sorted_targets
            .into_iter()
            .filter_map(|hash| {
                vertices_by_hash
                    .get(&hash)
                    .map(|v| (v.active_package.clone(), Self::effective_tokens(v)))
            })
            .collect();

        let mut h_cache: HashMap<u64, f64> =
            HashMap::with_capacity(vertices_by_hash.len().min(64));
        let mut heuristic = |vertex_hash: u64| -> f64 {
            if valid_targets.contains(&vertex_hash) {
                return 0.0;
            }
            if let Some(&cached) = h_cache.get(&vertex_hash) {
                return cached;
            }
            let Some(vertex) = vertices_by_hash.get(&vertex_hash) else {
                h_cache.insert(vertex_hash, self.configuration.min_step_cost);
                return self.configuration.min_step_cost;
            };
            let u_pkg = vertex.active_package.as_str();
            let u_tokens = Self::effective_tokens(vertex);

            let mut best_sim = 0.0_f64;
            for (g_pkg, g_tokens) in &goal_descriptors {
                if g_pkg != u_pkg {
                    continue;
                }
                if u_tokens.is_empty() && g_tokens.is_empty() {
                    best_sim = 1.0;
                    break;
                }
                if u_tokens.is_empty() || g_tokens.is_empty() {
                    continue;
                }
                let min_c = u_tokens.len().min(g_tokens.len());
                let max_c = u_tokens.len().max(g_tokens.len());
                if (min_c as f64 / max_c as f64) <= best_sim {
                    continue;
                }
                let sim = ScreenLayoutHasher::jaccard_similarity(&u_tokens, g_tokens);
                if sim > best_sim {
                    best_sim = sim;
                    if best_sim >= 1.0 {
                        break;
                    }
                }
            }

            let h = (1.0 - best_sim) * self.configuration.min_step_cost;
            h_cache.insert(vertex_hash, h);
            h
        };

        let mut open_queue = BinaryHeap::with_capacity(vertices_by_hash.len().min(128));
        let start_key = AStarStateKey {
            vertex_hash: start_hash,
            hop_count: 0,
        };
        let mut best_g: HashMap<AStarStateKey, f64> = HashMap::new();
        best_g.insert(start_key, 0.0);
        let mut pareto_front: HashMap<u64, Vec<(f64, usize)>> = HashMap::new();
        pareto_front.insert(start_hash, vec![(0.0, 0)]);
        let mut came_from: HashMap<AStarStateKey, &ScreenTransitionEdge> = HashMap::new();
        let mut closed: HashSet<AStarStateKey> = HashSet::new();

        let start_h = heuristic(start_hash);
        open_queue.push(AStarOpenNode {
            vertex_hash: start_hash,
            g_score: 0.0,
            h_score: start_h,
            f_score: start_h,
            hop_count: 0,
        });

        let mut expanded_count = 0usize;

        while let Some(current) = open_queue.pop() {
            let current_key = AStarStateKey {
                vertex_hash: current.vertex_hash,
                hop_count: current.hop_count,
            };
            if closed.contains(&current_key) {
                continue;
            }
            if current.g_score > best_g.get(&current_key).copied().unwrap_or(f64::INFINITY) {
                continue;
            }

            if valid_targets.contains(&current.vertex_hash) {
                let path = Self::reconstruct_path(start_hash, current_key, &came_from);
                return Some(AStarSearchResult {
                    start_hash,
                    target_hash: current.vertex_hash,
                    edges: path,
                    total_cost: current.g_score,
                    expanded_node_count: expanded_count,
                });
            }

            closed.insert(current_key);
            if expanded_count >= self.configuration.max_node_expansions {
                break;
            }
            expanded_count += 1;

            if current.hop_count >= self.configuration.max_steps {
                continue;
            }
            let Some(edges) = outgoing.get(&current.vertex_hash) else {
                continue;
            };

            let next_hop = current.hop_count + 1;
            for &edge in edges {
                let next_hash = edge.to_hash;
                if !vertices_by_hash.contains_key(&next_hash) {
                    continue;
                }
                let next_key = AStarStateKey {
                    vertex_hash: next_hash,
                    hop_count: next_hop,
                };
                if closed.contains(&next_key) {
                    continue;
                }

                let step_cost = self.edge_cost(edge);
                let tentative_g = current.g_score + step_cost;

                let front = pareto_front.entry(next_hash).or_default();
                if front.iter().any(|&(g, hop)| g <= tentative_g && hop <= next_hop) {
                    continue;
                }
                front.retain(|&(g, hop)| !(tentative_g <= g && next_hop <= hop));
                front.push((tentative_g, next_hop));

                best_g.insert(next_key, tentative_g);
                came_from.insert(next_key, edge);
                let h = heuristic(next_hash);
                open_queue.push(AStarOpenNode {
                    vertex_hash: next_hash,
                    g_score: tentative_g,
                    h_score: h,
                    f_score: tentative_g + h,
                    hop_count: next_hop,
                });
            }
        }

        None
    }

    fn edge_cost(&self, edge: &ScreenTransitionEdge) -> f64 {
        let capped_ms = edge.average_transition_ms.clamp(0.0, 10_000.0);
        let latency_term = self.configuration.latency_weight_per_second * (capped_ms / 1000.0);
        let reliability_term = if edge.traversal_count >= 3 {
            0.0
        } else {
            0.03 / (edge.traversal_count.max(1) as f64)
        };
        self.configuration.min_step_cost + latency_term + reliability_term
    }

    fn effective_tokens(vertex: &ScreenStateVertex) -> BTreeSet<String> {
        if !vertex.semantic_tokens.is_empty() {
            return vertex.semantic_tokens.clone();
        }
        let mut fallback = BTreeSet::new();
        if !vertex.active_package.is_empty() {
            fallback.insert(format!("pkg|{}", vertex.active_package));
        }
        for label in &vertex.sample_interactive_labels {
            if !label.is_empty() {
                fallback.insert(format!("lbl|{}", ScreenLayoutHasher::normalize_label(label)));
            }
        }
        fallback
    }

    fn reconstruct_path(
        start_hash: u64,
        target_key: AStarStateKey,
        came_from: &HashMap<AStarStateKey, &ScreenTransitionEdge>,
    ) -> Vec<ScreenTransitionEdge> {
        let mut reversed = Vec::new();
        let mut visited = HashSet::new();
        let mut cursor = target_key;
        while !(cursor.vertex_hash == start_hash && cursor.hop_count == 0)
            && visited.insert(cursor)
        {
            let Some(&edge) = came_from.get(&cursor) else {
                break;
            };
            reversed.push(edge.clone());
            cursor = AStarStateKey {
                vertex_hash: edge.from_hash,
                hop_count: cursor.hop_count.saturating_sub(1),
            };
        }
        reversed.reverse();
        reversed
    }
}
