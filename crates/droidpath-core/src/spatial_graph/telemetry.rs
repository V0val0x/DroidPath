use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::protocol::ShadowUINode;
use crate::shadow_dom::ShadowTreeIndex;
use crate::spatial_graph::ScreenLayoutHasher;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackCandidate {
    pub node_id: i64,
    pub label: String,
    pub resource_id: String,
    pub class_name: String,
    pub bounds: String,
    pub center_x: i32,
    pub center_y: i32,
    pub clickable: bool,
    pub scrollable: bool,
    pub similarity_score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedNodeSummary {
    pub node_id: i64,
    pub change_kind: String,
    pub label: String,
    pub resource_id: String,
    pub clickable: bool,
    pub bounds: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenDeltaSummary {
    pub previous_hash: Option<String>,
    pub current_hash: Option<String>,
    pub screen_changed: bool,
    pub added_count: usize,
    pub removed_count: usize,
    pub updated_count: usize,
    pub changed_nodes: Vec<ChangedNodeSummary>,
}

/// Structured, PII-redacted self-correction telemetry returned to autonomous AI agents
/// on action failure, element-not-found, or route deviation so they can recover in 1 turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgenticFeedback {
    pub failure_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_target: Option<String>,
    pub current_screen_hash: Option<String>,
    pub current_package: String,
    pub is_secure_screen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_delta: Option<ScreenDeltaSummary>,
    pub closest_candidates: Vec<FeedbackCandidate>,
    pub visible_interactive_count: usize,
    pub scrollable_available: bool,
    pub suggested_next_actions: Vec<String>,
}

pub struct AgenticFeedbackBuilder;

impl AgenticFeedbackBuilder {
    const MAX_CANDIDATES: usize = 8;
    const MAX_CHANGED_NODES: usize = 12;

    pub fn build(
        failure_code: &str,
        requested_target: Option<&str>,
        previous_index: Option<&ShadowTreeIndex>,
        current_index: Option<&ShadowTreeIndex>,
        is_secure_screen: bool,
    ) -> AgenticFeedback {
        let current_package = current_index
            .map(|idx| idx.active_package.clone())
            .unwrap_or_default();
        let hasher = ScreenLayoutHasher::default();
        let current_hash_u64 = current_index.map(|idx| hasher.fingerprint(idx).layout_hash);
        let current_screen_hash = current_hash_u64.map(|h| format!("0x{h:016x}"));
        let redacted_target = requested_target
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ScreenLayoutHasher::redact_pii);

        if is_secure_screen {
            return AgenticFeedback {
                failure_code: failure_code.to_string(),
                requested_target: redacted_target,
                current_screen_hash,
                current_package,
                is_secure_screen: true,
                screen_delta: None,
                closest_candidates: Vec::new(),
                visible_interactive_count: 0,
                scrollable_available: false,
                suggested_next_actions: vec![
                    "FLAG_SECURE / biometric screen is active. Ask the user to unlock on the device and retry."
                        .to_string(),
                ],
            };
        }

        let Some(curr) = current_index else {
            return AgenticFeedback {
                failure_code: failure_code.to_string(),
                requested_target: redacted_target,
                current_screen_hash: None,
                current_package,
                is_secure_screen: false,
                screen_delta: None,
                closest_candidates: Vec::new(),
                visible_interactive_count: 0,
                scrollable_available: false,
                suggested_next_actions: vec![
                    "Shadow DOM snapshot is unavailable. Call android_get_ui_tree to inspect the screen."
                        .to_string(),
                ],
            };
        };

        let visible_nodes: Vec<&ShadowUINode> = curr
            .ordered_nodes
            .iter()
            .filter(|n| n.visible && n.enabled)
            .collect();

        let visible_interactive_count = visible_nodes
            .iter()
            .filter(|n| n.clickable || n.editable || n.scrollable)
            .count();
        let scrollable_available = visible_nodes.iter().any(|n| n.scrollable);

        let screen_delta = previous_index.map(|prev| Self::compute_delta(prev, curr));
        let closest_candidates =
            Self::rank_candidates(&visible_nodes, redacted_target.as_deref());
        let suggested_next_actions = Self::build_suggestions(
            failure_code,
            redacted_target.as_deref(),
            &closest_candidates,
            scrollable_available,
            screen_delta.as_ref(),
        );

        AgenticFeedback {
            failure_code: failure_code.to_string(),
            requested_target: redacted_target,
            current_screen_hash,
            current_package,
            is_secure_screen: false,
            screen_delta,
            closest_candidates,
            visible_interactive_count,
            scrollable_available,
            suggested_next_actions,
        }
    }

    fn redacted_label(node: &ShadowUINode) -> String {
        let raw = if !node.text.trim().is_empty() {
            node.text.trim()
        } else if !node.content_desc.trim().is_empty() {
            node.content_desc.trim()
        } else if !node.resource_id.trim().is_empty() {
            node.resource_id
                .rsplit('/')
                .next()
                .unwrap_or(node.resource_id.as_str())
        } else {
            ""
        };
        if raw.is_empty() {
            String::new()
        } else {
            ScreenLayoutHasher::redact_pii(raw)
        }
    }

    fn compute_delta(prev: &ShadowTreeIndex, curr: &ShadowTreeIndex) -> ScreenDeltaSummary {
        let hasher = ScreenLayoutHasher::default();
        let prev_hash = hasher.fingerprint(prev).layout_hash;
        let curr_hash = hasher.fingerprint(curr).layout_hash;

        let prev_map: HashMap<i64, &ShadowUINode> =
            prev.ordered_nodes.iter().map(|n| (n.node_id, n)).collect();
        let curr_ids: HashSet<i64> = curr.ordered_nodes.iter().map(|n| n.node_id).collect();

        let removed_count = prev
            .ordered_nodes
            .iter()
            .filter(|n| !curr_ids.contains(&n.node_id))
            .count();

        let mut added_count = 0usize;
        let mut updated_count = 0usize;
        let mut changed_nodes: Vec<ChangedNodeSummary> = Vec::new();

        for node in &curr.ordered_nodes {
            if !node.visible {
                continue;
            }
            let label = Self::redacted_label(node);
            match prev_map.get(&node.node_id) {
                None => {
                    added_count += 1;
                    if (!label.is_empty() || node.clickable)
                        && changed_nodes.len() < Self::MAX_CHANGED_NODES
                    {
                        changed_nodes.push(ChangedNodeSummary {
                            node_id: node.node_id,
                            change_kind: "added".to_string(),
                            label,
                            resource_id: node.resource_id.clone(),
                            clickable: node.clickable,
                            bounds: node.bounds.clone(),
                        });
                    }
                }
                Some(old) => {
                    if old.text != node.text
                        || old.content_desc != node.content_desc
                        || old.bounds != node.bounds
                        || old.clickable != node.clickable
                    {
                        updated_count += 1;
                        if (!label.is_empty() || node.clickable)
                            && changed_nodes.len() < Self::MAX_CHANGED_NODES
                        {
                            changed_nodes.push(ChangedNodeSummary {
                                node_id: node.node_id,
                                change_kind: "updated".to_string(),
                                label,
                                resource_id: node.resource_id.clone(),
                                clickable: node.clickable,
                                bounds: node.bounds.clone(),
                            });
                        }
                    }
                }
            }
        }

        ScreenDeltaSummary {
            previous_hash: Some(format!("0x{prev_hash:016x}")),
            current_hash: Some(format!("0x{curr_hash:016x}")),
            screen_changed: prev_hash != curr_hash,
            added_count,
            removed_count,
            updated_count,
            changed_nodes,
        }
    }

    fn rank_candidates(
        visible_nodes: &[&ShadowUINode],
        query: Option<&str>,
    ) -> Vec<FeedbackCandidate> {
        let query_lower = query.map(|q| q.to_lowercase());
        let query_tokens: Vec<&str> = query_lower
            .as_deref()
            .map(|q| {
                q.split(|c: char| !c.is_alphanumeric())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let mut scored: Vec<FeedbackCandidate> = visible_nodes
            .iter()
            .filter_map(|node| {
                let label = Self::redacted_label(node);
                if label.is_empty() && !node.clickable && !node.editable {
                    return None;
                }
                let score = Self::score_node(
                    &label,
                    &node.resource_id,
                    node.clickable,
                    node.editable,
                    query_lower.as_deref(),
                    &query_tokens,
                );
                Some(FeedbackCandidate {
                    node_id: node.node_id,
                    label,
                    resource_id: node.resource_id.clone(),
                    class_name: node.class_name.clone(),
                    bounds: node.bounds.clone(),
                    center_x: node.center_x,
                    center_y: node.center_y,
                    clickable: node.clickable,
                    scrollable: node.scrollable,
                    similarity_score: score,
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            b.similarity_score
                .cmp(&a.similarity_score)
                .then_with(|| b.clickable.cmp(&a.clickable))
                .then_with(|| a.node_id.cmp(&b.node_id))
        });

        // Deduplicate identical (label, center_x, center_y) pairs so agents see diverse options.
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(Self::MAX_CANDIDATES);
        for cand in scored {
            let key = (cand.label.to_lowercase(), cand.center_x, cand.center_y);
            if seen.insert(key) {
                out.push(cand);
                if out.len() >= Self::MAX_CANDIDATES {
                    break;
                }
            }
        }
        out
    }

    fn score_node(
        label: &str,
        resource_id: &str,
        clickable: bool,
        editable: bool,
        query_lower: Option<&str>,
        query_tokens: &[&str],
    ) -> i32 {
        let mut score = if clickable || editable { 50 } else { 0 };
        let Some(q) = query_lower else {
            return score + if !label.is_empty() { 100 } else { 0 };
        };

        let label_low = label.to_lowercase();
        let rid_low = resource_id.to_lowercase();

        if !label_low.is_empty() {
            if label_low == q {
                score += 950;
            } else if label_low.starts_with(q) {
                score += 800;
            } else if label_low.contains(q) || q.contains(&label_low) {
                score += 650;
            } else if !query_tokens.is_empty() {
                let matched = query_tokens
                    .iter()
                    .filter(|t| label_low.contains(**t))
                    .count();
                if matched > 0 {
                    score += ((matched as i32 * 500) / (query_tokens.len() as i32)).min(500);
                }
            }
        }

        if !rid_low.is_empty() {
            if rid_low.ends_with(q) || rid_low.contains(q) {
                score += 300;
            } else if !query_tokens.is_empty() {
                let matched = query_tokens
                    .iter()
                    .filter(|t| rid_low.contains(**t))
                    .count();
                if matched > 0 {
                    score += ((matched as i32 * 200) / (query_tokens.len() as i32)).min(200);
                }
            }
        }

        score.min(1000)
    }

    fn build_suggestions(
        failure_code: &str,
        requested_target: Option<&str>,
        candidates: &[FeedbackCandidate],
        scrollable_available: bool,
        delta: Option<&ScreenDeltaSummary>,
    ) -> Vec<String> {
        let mut tips = Vec::new();
        if let Some(top) = candidates.first() {
            if top.similarity_score >= 300 && !top.label.is_empty() {
                tips.push(format!(
                    "Tap closest visible candidate '{}' at ({}, {}) via android_tap_element.",
                    top.label, top.center_x, top.center_y
                ));
            }
        }
        if scrollable_available {
            let target_hint = requested_target.unwrap_or("<target>");
            tips.push(format!(
                "Scrollable container detected on screen; try android_explore_and_click(target=\"{target_hint}\", max_scrolls=5)."
            ));
        }
        if let Some(d) = delta {
            if d.added_count > 0 {
                tips.push(format!(
                    "{} new UI node(s) appeared (possible dialog/overlay); inspect feedback.screen_delta.changed_nodes before retrying.",
                    d.added_count
                ));
            }
        }
        if failure_code == "route_deviation" || failure_code == "no_route" {
            tips.push(
                "Retry with android_navigate_to_node(auto_explore=true) or press BACK via android_keyevent(keycode=\"BACK\")."
                    .to_string(),
            );
        }
        if tips.is_empty() {
            tips.push(
                "Call android_get_ui_tree(include_vision=true) if visual disambiguation is needed."
                    .to_string(),
            );
        }
        tips
    }
}
