use std::collections::HashSet;

use crate::protocol::ShadowUINode;
use crate::shadow_dom::ShadowTreeIndex;
use crate::spatial_graph::HazardousActionClassifier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DismissCandidate {
    pub node_id: i64,
    pub tap_x: i32,
    pub tap_y: i32,
    pub label: String,
    pub reason: String,
    pub score: i32,
}

const LAUNCHER_PACKAGES: &[&str] = &[
    "com.google.android.apps.nexuslauncher",
    "com.android.launcher3",
    "com.android.launcher",
    "com.sec.android.app.launcher",
    "com.miui.home",
    "com.oppo.launcher",
    "com.oneplus.launcher",
];

const EXACT_DISMISS_PHRASES: &[(&str, i32)] = &[
    ("не сейчас", 115),
    ("not now", 115),
    ("позже", 105),
    ("maybe later", 105),
    ("later", 100),
    ("напомнить позже", 105),
    ("закрыть", 110),
    ("close", 110),
    ("пропустить", 108),
    ("skip", 108),
    ("нет, спасибо", 112),
    ("no thanks", 112),
    ("no, thanks", 112),
    ("отклонить", 102),
    ("decline", 102),
    ("dismiss", 110),
    ("понятно", 96),
    ("got it", 96),
    ("скрыть", 98),
    ("hide", 95),
    ("не разрешать", 98),
    ("don't allow", 98),
    ("deny", 96),
];

const COMPACT_CLOSE_GLYPHS: &[&str] = &[
    "x",
    "×",
    "✕",
    "✖",
    "╳",
    "close ad",
    "закрыть рекламу",
];

const DISMISS_RESOURCE_ID_SUFFIXES: &[(&str, i32)] = &[
    ("interstitial_close", 120),
    ("close_button", 115),
    ("btn_close", 115),
    ("close_btn", 115),
    ("iv_close", 110),
    ("ic_close", 108),
    ("dismiss", 105),
    ("dismiss_button", 108),
    ("btn_dismiss", 108),
    ("skip_button", 108),
    ("btn_skip", 108),
    ("negative_button", 96),
    ("close", 92),
];

/// Deterministic detector for closing non-destructive in-app popups, cookie prompts,
/// and promo splash screens during A* auto-recovery (#4).
pub struct UniversalDismissDetector;

impl UniversalDismissDetector {
    pub fn find_dismiss_candidate(
        index: &ShadowTreeIndex,
        excluding_node_ids: &HashSet<i64>,
        ignored_packages: &HashSet<String>,
    ) -> Option<DismissCandidate> {
        if LAUNCHER_PACKAGES.contains(&index.active_package.as_str()) {
            return None;
        }

        let mut best: Option<DismissCandidate> = None;

        for node in &index.ordered_nodes {
            if !node.visible || !node.enabled {
                continue;
            }
            if excluding_node_ids.contains(&node.node_id) {
                continue;
            }
            if !node.package_name.is_empty() && ignored_packages.contains(&node.package_name) {
                continue;
            }
            if HazardousActionClassifier::assess_node(node).is_hazardous {
                continue;
            }

            let resolved_target = Self::clickable_target(node, index);
            if excluding_node_ids.contains(&resolved_target.node_id) {
                continue;
            }
            if HazardousActionClassifier::assess_node(resolved_target).is_hazardous {
                continue;
            }

            let Some((tap_x, tap_y)) = Self::tap_point(resolved_target) else {
                continue;
            };
            let Some((score, label, reason)) = Self::match_score(node, resolved_target) else {
                continue;
            };

            let candidate = DismissCandidate {
                node_id: resolved_target.node_id,
                tap_x,
                tap_y,
                label,
                reason,
                score,
            };
            match &best {
                Some(current_best) if candidate.score <= current_best.score => {}
                _ => best = Some(candidate),
            }
        }

        best
    }

    fn match_score(
        node: &ShadowUINode,
        tap_target: &ShadowUINode,
    ) -> Option<(i32, String, String)> {
        let clickable_bonus = if tap_target.clickable { 15 } else { 0 };

        for raw in [&node.text, &node.content_desc] {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lowered = trimmed.to_lowercase();

            if let Some(&(_, base_score)) =
                EXACT_DISMISS_PHRASES.iter().find(|&&(p, _)| p == lowered)
            {
                return Some((
                    base_score + clickable_bonus,
                    trimmed.to_string(),
                    format!("phrase:{lowered}"),
                ));
            }

            if COMPACT_CLOSE_GLYPHS.contains(&lowered.as_str()) {
                let rect = ShadowTreeIndex::parse_bounds_rect(&tap_target.bounds)
                    .or_else(|| ShadowTreeIndex::parse_bounds_rect(&node.bounds))?;
                let width = rect.max_x - rect.min_x;
                let height = rect.max_y - rect.min_y;
                if width <= 220 && height <= 220 {
                    return Some((
                        104 + clickable_bonus,
                        trimmed.to_string(),
                        format!("close_glyph:{lowered}"),
                    ));
                }
            }
        }

        if !node.resource_id.is_empty() {
            let short_id = ShadowTreeIndex::extract_short_resource_id(&node.resource_id)
                .to_lowercase();
            if let Some(&(_, rid_score)) = DISMISS_RESOURCE_ID_SUFFIXES
                .iter()
                .find(|&&(s, _)| s == short_id)
            {
                let label = if !node.content_desc.is_empty() {
                    node.content_desc.clone()
                } else if !node.text.is_empty() {
                    node.text.clone()
                } else {
                    short_id.clone()
                };
                return Some((
                    rid_score + clickable_bonus,
                    label,
                    format!("resource_id:{short_id}"),
                ));
            }
        }

        None
    }

    fn clickable_target<'a>(node: &'a ShadowUINode, index: &'a ShadowTreeIndex) -> &'a ShadowUINode {
        if node.clickable {
            return node;
        }
        let mut cursor_id = node.parent_id;
        let mut depth = 0;
        while let Some(pid) = cursor_id {
            if depth >= 3 {
                break;
            }
            let Some(parent) = index.nodes_by_id.get(&pid) else {
                break;
            };
            if parent.clickable && parent.visible && parent.enabled {
                return parent;
            }
            cursor_id = parent.parent_id;
            depth += 1;
        }
        node
    }

    fn tap_point(node: &ShadowUINode) -> Option<(i32, i32)> {
        if node.center_x > 0 || node.center_y > 0 {
            return Some((node.center_x, node.center_y));
        }
        let rect = ShadowTreeIndex::parse_bounds_rect(&node.bounds)?;
        Some(((rect.min_x + rect.max_x) / 2, (rect.min_y + rect.max_y) / 2))
    }
}
