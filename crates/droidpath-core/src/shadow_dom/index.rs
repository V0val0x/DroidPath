use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::protocol::{ShadowCompactNode, ShadowUINode};
use crate::spatial_graph::{AttributedUIAction, UIActionKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundsRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

impl BoundsRect {
    #[inline]
    pub fn area(&self) -> i64 {
        i64::from(self.max_x - self.min_x) * i64::from(self.max_y - self.min_y)
    }

    #[inline]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }
}

/// Immutable, pre-indexed snapshot of the active Android UI tree for O(1) lookups and zero-copy reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowTreeIndex {
    pub seq: u64,
    pub timestamp_ms: i64,
    pub active_package: String,
    pub active_window_id: i32,
    pub ordered_nodes: Vec<ShadowUINode>,
    pub compact_nodes: Vec<ShadowCompactNode>,
    pub nodes_by_id: HashMap<i64, ShadowUINode>,
    pub children_by_parent_id: HashMap<i64, Vec<i64>>,

    nodes_by_resource_id: HashMap<String, Vec<usize>>,
    nodes_by_short_resource_id: HashMap<String, Vec<usize>>,
    nodes_by_lowercased_text: HashMap<String, Vec<usize>>,
    nodes_by_lowercased_desc: HashMap<String, Vec<usize>>,
}

impl ShadowTreeIndex {
    pub fn new(
        seq: u64,
        timestamp_ms: i64,
        active_package: impl Into<String>,
        active_window_id: i32,
        mut nodes: Vec<ShadowUINode>,
    ) -> Self {
        nodes.sort_by(|a, b| a.index.cmp(&b.index).then_with(|| a.node_id.cmp(&b.node_id)));

        let count = nodes.len();
        let mut compact_nodes = Vec::with_capacity(count);
        let mut nodes_by_id = HashMap::with_capacity(count);
        let mut children_by_parent_id: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut nodes_by_resource_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut nodes_by_short_resource_id: HashMap<String, Vec<usize>> = HashMap::new();
        let mut nodes_by_lowercased_text: HashMap<String, Vec<usize>> = HashMap::new();
        let mut nodes_by_lowercased_desc: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, node) in nodes.iter().enumerate() {
            compact_nodes.push(node.to_compact_node());
            nodes_by_id.insert(node.node_id, node.clone());

            if let Some(pid) = node.parent_id {
                children_by_parent_id
                    .entry(pid)
                    .or_default()
                    .push(node.node_id);
            }
            if !node.resource_id.is_empty() {
                nodes_by_resource_id
                    .entry(node.resource_id.clone())
                    .or_default()
                    .push(idx);
                let short_id = Self::extract_short_resource_id(&node.resource_id);
                if !short_id.is_empty() {
                    nodes_by_short_resource_id
                        .entry(short_id.to_string())
                        .or_default()
                        .push(idx);
                }
            }
            if !node.text.is_empty() {
                nodes_by_lowercased_text
                    .entry(node.text.to_lowercase())
                    .or_default()
                    .push(idx);
            }
            if !node.content_desc.is_empty() {
                nodes_by_lowercased_desc
                    .entry(node.content_desc.to_lowercase())
                    .or_default()
                    .push(idx);
            }
        }

        Self {
            seq,
            timestamp_ms,
            active_package: active_package.into(),
            active_window_id,
            ordered_nodes: nodes,
            compact_nodes,
            nodes_by_id,
            children_by_parent_id,
            nodes_by_resource_id,
            nodes_by_short_resource_id,
            nodes_by_lowercased_text,
            nodes_by_lowercased_desc,
        }
    }

    pub fn exact_nodes_for_resource_id(&self, resource_id: &str) -> Option<Vec<&ShadowUINode>> {
        self.nodes_by_resource_id
            .get(resource_id)
            .map(|idxs| idxs.iter().map(|&i| &self.ordered_nodes[i]).collect())
    }

    pub fn short_id_nodes(&self, short_id: &str) -> Option<Vec<&ShadowUINode>> {
        self.nodes_by_short_resource_id
            .get(short_id)
            .map(|idxs| idxs.iter().map(|&i| &self.ordered_nodes[i]).collect())
    }

    pub fn exact_lowercased_text_nodes(&self, query_lower: &str) -> Option<Vec<&ShadowUINode>> {
        self.nodes_by_lowercased_text
            .get(query_lower)
            .map(|idxs| idxs.iter().map(|&i| &self.ordered_nodes[i]).collect())
    }

    pub fn exact_lowercased_desc_nodes(&self, query_lower: &str) -> Option<Vec<&ShadowUINode>> {
        self.nodes_by_lowercased_desc
            .get(query_lower)
            .map(|idxs| idxs.iter().map(|&i| &self.ordered_nodes[i]).collect())
    }

    /// Parses UIAutomator / Accessibility `[x1,y1][x2,y2]` bounds into integer coordinates.
    pub fn parse_bounds_rect(bounds: &str) -> Option<BoundsRect> {
        let mut nums = Vec::with_capacity(4);
        let mut current = String::new();

        for ch in bounds.chars() {
            if ch == '-' {
                if current.is_empty() {
                    current.push(ch);
                } else {
                    if let Ok(val) = current.parse::<i32>() {
                        nums.push(val);
                    }
                    current.clear();
                    current.push('-');
                }
            } else if ch.is_ascii_digit() {
                current.push(ch);
            } else if !current.is_empty() {
                if let Ok(val) = current.parse::<i32>() {
                    nums.push(val);
                }
                current.clear();
            }
        }
        if !current.is_empty() {
            if let Ok(val) = current.parse::<i32>() {
                nums.push(val);
            }
        }

        if nums.len() != 4 {
            return None;
        }
        let min_x = nums[0].min(nums[2]);
        let min_y = nums[1].min(nums[3]);
        let max_x = nums[0].max(nums[2]);
        let max_y = nums[1].max(nums[3]);
        if max_x <= min_x || max_y <= min_y {
            return None;
        }
        Some(BoundsRect {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    /// Resolves the most specific visible UI node under physical display coordinates `(x, y)`
    /// for semantic tap attribution (`selector_text`, `selector_content_desc`, `selector_resource_id`).
    pub fn resolve_node_at(&self, x: i32, y: i32) -> Option<&ShadowUINode> {
        let mut best_node: Option<&ShadowUINode> = None;
        let mut best_bounds: Option<BoundsRect> = None;

        for node in &self.ordered_nodes {
            if !node.visible {
                continue;
            }
            let has_label = !node.text.is_empty() || !node.content_desc.is_empty();
            let is_interactive = node.clickable || node.editable;
            if !has_label && !is_interactive {
                continue;
            }
            let Some(rect) = Self::parse_bounds_rect(&node.bounds) else {
                continue;
            };
            if !rect.contains(x, y) {
                continue;
            }

            if let (Some(current_best), Some(current_rect)) = (best_node, best_bounds) {
                let lhs_area = rect.area();
                let rhs_area = current_rect.area();
                let should_replace = if lhs_area < rhs_area {
                    true
                } else if lhs_area == rhs_area {
                    let current_has_label =
                        !current_best.text.is_empty() || !current_best.content_desc.is_empty();
                    if has_label != current_has_label {
                        has_label
                    } else if node.clickable != current_best.clickable {
                        node.clickable
                    } else if node.depth != current_best.depth {
                        node.depth > current_best.depth
                    } else {
                        node.index > current_best.index
                    }
                } else {
                    false
                };
                if should_replace {
                    best_node = Some(node);
                    best_bounds = Some(rect);
                }
            } else {
                best_node = Some(node);
                best_bounds = Some(rect);
            }
        }

        let winner = best_node?;

        if winner.text.is_empty() && winner.content_desc.is_empty() && winner.clickable {
            if let Some(descendant) = self.find_best_semantic_descendant(winner.node_id) {
                return Some(descendant);
            }
        }
        Some(winner)
    }

    /// Builds an `AttributedUIAction` for a manual tap at physical display coordinates `(x, y)`.
    pub fn attributed_tap_action(
        &self,
        x: i32,
        y: i32,
        timestamp: DateTime<Utc>,
    ) -> AttributedUIAction {
        let Some(node) = self.resolve_node_at(x, y) else {
            return AttributedUIAction {
                kind: UIActionKind::Tap,
                target_description: format!("({x},{y})"),
                x: Some(x),
                y: Some(y),
                end_x: None,
                end_y: None,
                duration_ms: None,
                text_input: None,
                press_enter: None,
                selector_resource_id: None,
                selector_text: None,
                selector_content_desc: None,
                timestamp,
            };
        };

        let label = if !node.text.is_empty() {
            node.text.clone()
        } else if !node.content_desc.is_empty() {
            node.content_desc.clone()
        } else if !node.resource_id.is_empty() {
            node.resource_id.clone()
        } else {
            format!("({x},{y})")
        };

        AttributedUIAction {
            kind: UIActionKind::Tap,
            target_description: label,
            x: Some(x),
            y: Some(y),
            end_x: None,
            end_y: None,
            duration_ms: None,
            text_input: None,
            press_enter: None,
            selector_resource_id: (!node.resource_id.is_empty())
                .then(|| node.resource_id.clone()),
            selector_text: (!node.text.is_empty()).then(|| node.text.clone()),
            selector_content_desc: (!node.content_desc.is_empty())
                .then(|| node.content_desc.clone()),
            timestamp,
        }
    }

    fn find_best_semantic_descendant(&self, root_id: i64) -> Option<&ShadowUINode> {
        let initial_children = self.children_by_parent_id.get(&root_id)?;
        if initial_children.is_empty() {
            return None;
        }
        let mut queue: VecDeque<i64> = initial_children.iter().copied().collect();
        let mut visited: HashSet<i64> = HashSet::new();
        visited.insert(root_id);
        let mut candidates: Vec<&ShadowUINode> = Vec::new();

        while let Some(child_id) = queue.pop_front() {
            if visited.len() >= 64 {
                break;
            }
            if !visited.insert(child_id) {
                continue;
            }
            let Some(child) = self.nodes_by_id.get(&child_id) else {
                continue;
            };
            if !child.visible {
                continue;
            }
            if !child.text.is_empty() || !child.content_desc.is_empty() {
                candidates.push(child);
            }
            if let Some(grandchildren) = self.children_by_parent_id.get(&child_id) {
                for &gid in grandchildren {
                    queue.push_back(gid);
                }
            }
        }

        candidates
            .into_iter()
            .max_by_key(|n| Self::descendant_label_score(n))
    }

    fn descendant_label_score(node: &ShadowUINode) -> (i32, i32) {
        let mut score = 0;
        if !node.text.is_empty() {
            score += 10;
        }
        if node.resource_id.to_lowercase().ends_with("title") {
            score += 8;
        }
        if !node.content_desc.is_empty() {
            score += 4;
        }
        if !node.resource_id.is_empty() {
            score += 2;
        }
        (score, -node.index)
    }

    pub fn extract_short_resource_id(resource_id: &str) -> &str {
        if let Some(idx) = resource_id.find(":id/") {
            return &resource_id[idx + 4..];
        }
        if let Some(idx) = resource_id.rfind('/') {
            if idx + 1 < resource_id.len() {
                return &resource_id[idx + 1..];
            }
        }
        resource_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDOMUpdate {
    pub index: std::sync::Arc<ShadowTreeIndex>,
    pub is_full_snapshot: bool,
    pub inserted_count: usize,
    pub updated_count: usize,
    pub removed_count: usize,
    pub event_type: Option<i32>,
    pub event_package: Option<String>,
    pub is_secure_screen_active: bool,
}
