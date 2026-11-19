use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use droidpath_core::*;
use tokio::sync::Mutex;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
fn make_node(
    node_id: i64,
    index: i32,
    package_name: &str,
    class_name: &str,
    resource_id: &str,
    text: &str,
    center_x: i32,
    center_y: i32,
    clickable: bool,
) -> ShadowUINode {
    ShadowUINode {
        index,
        text: text.to_string(),
        resource_id: resource_id.to_string(),
        content_desc: String::new(),
        class_name: class_name.to_string(),
        clickable,
        scrollable: false,
        bounds: format!(
            "[{},{}][{},{}]",
            (center_x - 50).max(0),
            (center_y - 25).max(0),
            center_x + 50,
            center_y + 25
        ),
        center_x,
        center_y,
        node_id,
        parent_id: None,
        window_id: 1,
        child_index: index,
        depth: 1,
        package_name: package_name.to_string(),
        editable: false,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

fn make_snapshot_msg(seq: u64, pkg: &str, nodes: Vec<ShadowUINode>) -> WireMessage {
    WireMessage::Snapshot(WireSnapshotPayload {
        seq,
        timestamp_ms: (seq * 100) as i64,
        reason: "test".to_string(),
        active_package: pkg.to_string(),
        active_window_id: 1,
        node_count: nodes.len(),
        nodes,
        secure_screen_active: None,
    })
}

fn make_update(seq: u64, pkg: &str, nodes: Vec<ShadowUINode>) -> ShadowDOMUpdate {
    let count = nodes.len();
    ShadowDOMUpdate {
        index: Arc::new(ShadowTreeIndex::new(
            seq,
            (seq * 100) as i64,
            pkg,
            1,
            nodes,
        )),
        is_full_snapshot: true,
        inserted_count: count,
        updated_count: 0,
        removed_count: 0,
        event_type: None,
        event_package: None,
        is_secure_screen_active: false,
    }
}

#[test]
fn test_astar_pathfinder_optimal_routing_filtering_max_steps_and_heuristic_steering() {
    let now = Utc::now();
    let v = |hash: u64, tokens: &[&str]| ScreenStateVertex {
        layout_hash: hash,
        active_package: "com.app".into(),
        sample_interactive_labels: vec![],
        semantic_tokens: tokens.iter().map(|s| (*s).to_string()).collect(),
        first_seen_at: now,
        last_seen_at: now,
        visit_count: 1,
        discovery_parent_hash: None,
        discovery_depth: 0,
    };
    let e = |from: u64, to: u64, label: Option<&str>, ms: f64, spanning: bool| {
        let action = label.map(|l| AttributedUIAction::tap(l, Some(100), Some(100)));
        ScreenTransitionEdge {
            id: Uuid::new_v4(),
            from_hash: from,
            to_hash: to,
            action_signature: action
                .as_ref()
                .map(|a| a.signature())
                .unwrap_or_else(|| "implicit".into()),
            action,
            traversal_count: 3,
            average_transition_ms: ms,
            is_spanning_tree_edge: spanning,
            last_traversed_at: now,
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
        }
    };

    let mut spanning = BTreeMap::new();
    spanning.insert(1, vec![2]);
    spanning.insert(2, vec![5]);
    let snap = SpatialGraphSnapshot {
        root_state_hash: Some(1),
        current_state_hash: Some(1),
        vertices: vec![
            v(1, &["a", "b"]),
            v(2, &["b", "c", "goal"]),
            v(3, &["d"]),
            v(4, &["e"]),
            v(5, &["b", "c", "goal", "target"]),
        ],
        edges: vec![
            e(1, 5, None, 10.0, false),
            e(1, 1, Some("Refresh"), 10.0, false),
            e(1, 2, Some("Fast"), 50.0, true),
            e(2, 5, Some("Finish"), 50.0, true),
            e(1, 3, Some("Detour"), 50.0, false),
            e(3, 4, Some("Mid"), 50.0, false),
            e(4, 5, Some("End"), 50.0, false),
        ],
        spanning_children: spanning,
    };

    let pf = AStarPathfinder::default();
    let res = pf.find_shortest_path(1, 5, &snap).unwrap();
    assert_eq!(
        res.edges.iter().map(|e| e.to_hash).collect::<Vec<_>>(),
        vec![2, 5]
    );
    assert!(res.edges.iter().all(|e| e.action.is_some()));

    assert!(
        AStarPathfinder::new(AStarCostConfiguration::with_max_steps(1))
            .find_shortest_path(1, 5, &snap)
            .is_none()
    );
    let already = pf.find_shortest_path(5, 5, &snap).unwrap();
    assert!(already.edges.is_empty());
    assert_eq!(already.total_cost, 0.0);

    // Heuristic steering check
    let tree_snap = SpatialGraphSnapshot {
        root_state_hash: Some(1),
        current_state_hash: Some(1),
        vertices: vec![
            v(1, &["root"]),
            v(10, &["shared1", "shared2", "shared3"]),
            v(20, &["unrelated"]),
            v(11, &["shared1", "shared2", "shared3", "dest"]),
            v(21, &["deadend"]),
        ],
        edges: vec![
            e(1, 20, Some("BranchB"), 800.0, false),
            e(20, 21, Some("DeepB"), 50.0, false),
            e(1, 10, Some("BranchA"), 50.0, false),
            e(10, 11, Some("Goal"), 50.0, false),
        ],
        spanning_children: BTreeMap::new(),
    };
    let steered = pf.find_shortest_path(1, 11, &tree_snap).unwrap();
    assert_eq!(
        steered.edges.iter().map(|e| e.to_hash).collect::<Vec<_>>(),
        vec![10, 11]
    );
    assert_eq!(steered.expanded_node_count, 2);
}

#[tokio::test]
async fn test_hazardous_hitl_pauses_before_payment_and_resumes_when_confirmed() {
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let home = vec![make_node(
        1, 0, "com.shop", "Button", "", "Checkout", 300, 400, true,
    )];
    let checkout = vec![
        make_node(2, 0, "com.shop", "TextView", "", "Order Summary", 300, 200, false),
        make_node(
            3,
            1,
            "com.shop",
            "Button",
            "com.shop:id/btn_pay",
            "Оплатить 15.99€",
            540,
            2100,
            true,
        ),
    ];
    let receipt = vec![make_node(
        4, 0, "com.shop", "TextView", "", "Receipt Confirmed", 300, 400, false,
    )];

    let hasher = ScreenLayoutHasher::default();
    let idx_checkout = ShadowTreeIndex::new(2, 200, "com.shop", 1, checkout.clone());
    let idx_receipt = ShadowTreeIndex::new(3, 300, "com.shop", 1, receipt.clone());
    let hash_checkout = hasher.fingerprint(&idx_checkout).layout_hash;
    let hash_receipt = hasher.fingerprint(&idx_receipt).layout_hash;

    bridge.graph_engine.process(&make_update(1, "com.shop", home.clone())).await;
    let mut act1 = AttributedUIAction::tap("Checkout", Some(300), Some(400));
    act1.selector_text = Some("Checkout".into());
    bridge.graph_engine.record_pending_action(act1).await;
    bridge.graph_engine.process(&make_update(2, "com.shop", checkout.clone())).await;

    let mut act2 = AttributedUIAction::tap("Оплатить 15.99€", Some(540), Some(2100));
    act2.selector_resource_id = Some("com.shop:id/btn_pay".into());
    act2.selector_text = Some("Оплатить 15.99€".into());
    bridge.graph_engine.record_pending_action(act2).await;
    bridge.graph_engine.process(&make_update(3, "com.shop", receipt.clone())).await;

    // Export & re-import must classify hazardous edges
    let exported = bridge.graph_engine.export_graph_json(false).await.unwrap();
    let reimported = SpatialGraphEngine::default();
    reimported.import_graph_json(&exported).await.unwrap();
    assert!(reimported.neighbors(hash_checkout).await[0].is_hazardous);

    bridge.dom_state.handle_message(make_snapshot_msg(10, "com.shop", home)).await;

    let mut ev_rx = bridge.subscribe_macro_events();

    struct PayExec {
        bridge: Arc<ShadowGraphBridge>,
        checkout: Vec<ShadowUINode>,
        receipt: Vec<ShadowUINode>,
        calls: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl MacroActionExecutor for PayExec {
        async fn execute(
            &self,
            action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            let sig = action.signature();
            self.calls.lock().await.push(sig.clone());
            if sig.contains("Checkout") {
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(11, "com.shop", self.checkout.clone()))
                    .await;
            } else {
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(12, "com.shop", self.receipt.clone()))
                    .await;
            }
            Ok(())
        }
    }

    let exec = Arc::new(PayExec {
        bridge: Arc::clone(&bridge),
        checkout,
        receipt,
        calls: Mutex::new(Vec::new()),
    });

    let first = bridge
        .route_and_execute(
            "61061JEA307110",
            exec.clone(),
            RouteExecuteOptions {
                target_hash: Some(hash_receipt),
                step_timeout_ms: 500,
                post_settle_quiescence_ms: 0,
                confirmed: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ShadowNavigationOutcome::RequiresConfirmation(req) = first else {
        panic!("Expected RequiresConfirmation, got {first:?}");
    };
    assert_eq!(*exec.calls.lock().await, vec!["tap:Checkout"]);
    assert_eq!(req.hazard_category, HazardCategory::Payment);
    assert_eq!(req.step_index, 1);
    assert_eq!(req.steps_completed, 1);
    assert_eq!(req.current_hash, hash_checkout);
    assert_eq!(req.tap_x, Some(540));
    assert_eq!(req.tap_y, Some(2100));

    let mut saw_awaiting = false;
    let mut saw_failed = false;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            MacroExecutionEvent::AwaitingConfirmation { .. } => saw_awaiting = true,
            MacroExecutionEvent::Failed { .. } => saw_failed = true,
            _ => {}
        }
    }
    assert!(saw_awaiting);
    assert!(!saw_failed);

    // Resume with confirmed = true
    let second = bridge
        .route_and_execute(
            "61061JEA307110",
            exec.clone(),
            RouteExecuteOptions {
                target_hash: Some(hash_receipt),
                step_timeout_ms: 500,
                post_settle_quiescence_ms: 0,
                confirmed: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ShadowNavigationOutcome::Arrived(report) = second else {
        panic!("Expected Arrived after confirmation, got {second:?}");
    };
    assert_eq!(report.final_hash, hash_receipt);
    assert!(report.recovered_steps.is_empty());
    let clean_formatted = DroidPathMcpServerCore::format_navigation_outcome(
        &ShadowNavigationOutcome::Arrived(report),
    );
    let clean_json: serde_json::Value = serde_json::from_str(&clean_formatted.text).unwrap();
    assert_eq!(clean_json["status"], "arrived");
    assert_eq!(clean_json["path_summary"][0]["step_index"], 0);
    assert_eq!(clean_json["path_summary"][0]["step"], 0);
    assert!(clean_json.get("recovered_steps").is_none());
    assert_eq!(exec.calls.lock().await.len(), 2);
}

#[tokio::test]
async fn test_reflexive_swipe_dismisses_occluding_banner_without_poisoning_graph() {
    let dom = Arc::new(ShadowDomState::default());
    let graph = Arc::new(SpatialGraphEngine::default());
    let hasher = ScreenLayoutHasher::default();

    let mut banner = make_node(
        900,
        1,
        "com.android.systemui",
        "FrameLayout",
        "com.android.systemui:id/expandableNotificationRow",
        "Push Alert",
        540,
        220,
        true,
    );
    banner.bounds = "[40,120][1040,320]".to_string();

    let start_clean = vec![make_node(
        1, 0, "com.app", "Button", "", "Compose", 500, 200, true,
    )];
    let start_with_banner = vec![start_clean[0].clone(), banner];
    let target = vec![make_node(
        2, 0, "com.app", "TextView", "", "New Draft", 300, 300, false,
    )];

    let idx_target = ShadowTreeIndex::new(2, 200, "com.app", 1, target.clone());
    let h_target = hasher.fingerprint(&idx_target).layout_hash;

    graph.process(&make_update(1, "com.app", start_clean.clone())).await;
    let mut act = AttributedUIAction::tap("Compose", Some(500), Some(200));
    act.selector_text = Some("Compose".into());
    graph.record_pending_action(act).await;
    let edge = graph.process(&make_update(2, "com.app", target.clone())).await.unwrap();
    let edges_before = graph.edge_count().await;

    dom.handle_message(make_snapshot_msg(10, "com.app", start_with_banner))
        .await;

    struct BannerExec {
        dom: Arc<ShadowDomState>,
        clean: Vec<ShadowUINode>,
        target: Vec<ShadowUINode>,
        actions: Mutex<Vec<AttributedUIAction>>,
    }
    #[async_trait]
    impl MacroActionExecutor for BannerExec {
        async fn execute(
            &self,
            action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            self.actions.lock().await.push(action.clone());
            if action.kind == UIActionKind::Swipe {
                self.dom
                    .handle_message(make_snapshot_msg(11, "com.app", self.clean.clone()))
                    .await;
            } else {
                self.dom
                    .handle_message(make_snapshot_msg(12, "com.app", self.target.clone()))
                    .await;
            }
            Ok(())
        }
    }

    let exec = Arc::new(BannerExec {
        dom: Arc::clone(&dom),
        clean: start_clean,
        target,
        actions: Mutex::new(Vec::new()),
    });
    let engine = MacroExecutionEngine::new(
        dom,
        Arc::clone(&graph),
        exec.clone(),
        hasher,
        MacroExecutionConfiguration {
            step_timeout_ms: 600,
            post_settle_quiescence_ms: 0,
            ..Default::default()
        },
        None,
    );

    let report = engine.execute(&[edge], "61061JEA307110").await.unwrap();
    assert_eq!(report.final_hash, h_target);
    let actions = exec.actions.lock().await;
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].kind, UIActionKind::Swipe);
    assert!(actions[0].y.unwrap() > actions[0].end_y.unwrap());
    assert_eq!(actions[1].kind, UIActionKind::Tap);
    assert_eq!(graph.edge_count().await, edges_before);
    assert!(report
        .recovered_steps
        .iter()
        .any(|s| s.starts_with("dismiss_notification_overlay")));
}

#[tokio::test]
async fn test_universal_dismiss_closes_promo_popup_and_refuses_hazardous_buttons() {
    let danger_idx = ShadowTreeIndex::new(
        1,
        100,
        "com.app",
        1,
        vec![
            make_node(1, 0, "com.app", "Button", "", "Close account", 500, 1200, true),
            make_node(2, 1, "com.app", "Button", "", "Удалить профиль", 500, 1400, true),
        ],
    );
    assert!(UniversalDismissDetector::find_dismiss_candidate(
        &danger_idx,
        &std::collections::HashSet::new(),
        &ScreenLayoutHasher::default().ignored_packages
    )
    .is_none());

    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let catalog = vec![make_node(
        1, 0, "com.shop", "Button", "", "Open Profile", 300, 400, true,
    )];
    let profile = vec![make_node(
        2, 0, "com.shop", "TextView", "", "Profile Ready", 300, 300, false,
    )];
    let promo = vec![
        make_node(10, 0, "com.shop", "TextView", "", "Subscribe 50% OFF!", 540, 800, false),
        make_node(
            11,
            1,
            "com.shop",
            "Button",
            "com.shop:id/btn_dismiss",
            "Не сейчас",
            540,
            1800,
            true,
        ),
    ];

    let idx_profile = ShadowTreeIndex::new(2, 200, "com.shop", 1, profile.clone());
    let h_profile = ScreenLayoutHasher::default().fingerprint(&idx_profile).layout_hash;

    bridge.graph_engine.process(&make_update(1, "com.shop", catalog.clone())).await;
    let mut act = AttributedUIAction::tap("Open Profile", Some(300), Some(400));
    act.selector_text = Some("Open Profile".into());
    bridge.graph_engine.record_pending_action(act).await;
    bridge.graph_engine.process(&make_update(2, "com.shop", profile.clone())).await;

    // Live device lands on unknown promo popup
    bridge.dom_state.handle_message(make_snapshot_msg(10, "com.shop", promo)).await;

    struct PromoExec {
        bridge: Arc<ShadowGraphBridge>,
        catalog: Vec<ShadowUINode>,
        profile: Vec<ShadowUINode>,
        taps: Mutex<Vec<(i32, i32)>>,
    }
    #[async_trait]
    impl MacroActionExecutor for PromoExec {
        async fn execute(
            &self,
            action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            let mut taps = self.taps.lock().await;
            taps.push((action.x.unwrap_or(-1), action.y.unwrap_or(-1)));
            if taps.len() == 1 {
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(11, "com.shop", self.catalog.clone()))
                    .await;
            } else {
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(12, "com.shop", self.profile.clone()))
                    .await;
            }
            Ok(())
        }
    }

    let exec = Arc::new(PromoExec {
        bridge: Arc::clone(&bridge),
        catalog,
        profile,
        taps: Mutex::new(Vec::new()),
    });
    let outcome = bridge
        .route_and_execute(
            "61061JEA307110",
            exec.clone(),
            RouteExecuteOptions {
                target_hash: Some(h_profile),
                step_timeout_ms: 600,
                post_settle_quiescence_ms: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ShadowNavigationOutcome::Arrived(report) = outcome else {
        panic!("Expected Arrived after Universal Dismiss, got {outcome:?}");
    };
    let taps = exec.taps.lock().await;
    assert_eq!(taps.len(), 2);
    assert_eq!(taps[0], (540, 1800));
    assert_eq!(report.final_hash, h_profile);
    assert!(report.recovered_steps.contains(&"universal_dismiss:Не сейчас".to_string()));
}

#[tokio::test]
async fn test_macro_execution_pauses_on_flag_secure_and_resumes_on_clear() {
    let dom = Arc::new(ShadowDomState::default());
    let graph = Arc::new(SpatialGraphEngine::default());
    let hasher = ScreenLayoutHasher::default();

    let start = vec![make_node(
        1, 0, "com.bank", "Button", "", "Transfer", 300, 400, true,
    )];
    let vault = vec![make_node(
        2, 0, "com.bank", "TextView", "", "Vault Unlocked", 300, 400, false,
    )];
    let h_vault = hasher
        .fingerprint(&ShadowTreeIndex::new(2, 200, "com.bank", 1, vault.clone()))
        .layout_hash;

    graph.process(&make_update(1, "com.bank", start.clone())).await;
    let mut act = AttributedUIAction::tap("Transfer", Some(300), Some(400));
    act.selector_text = Some("Transfer".into());
    graph.record_pending_action(act).await;
    let edge = graph.process(&make_update(2, "com.bank", vault.clone())).await.unwrap();

    dom.handle_message(make_snapshot_msg(10, "com.bank", start)).await;

    struct BioExec {
        dom: Arc<ShadowDomState>,
        vault: Vec<ShadowUINode>,
    }
    #[async_trait]
    impl MacroActionExecutor for BioExec {
        async fn execute(
            &self,
            _action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            self.dom
                .handle_message(WireMessage::SecureScreen(WireSecureScreenPayload {
                    code: "SECURE_SCREEN_ACTIVE".into(),
                    active: true,
                    reason: "biometric_prompt".into(),
                    active_package: "com.bank".into(),
                    timestamp_ms: 1100,
                }))
                .await;
            let dom_bg = Arc::clone(&self.dom);
            let vault_bg = self.vault.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(240)).await;
                dom_bg
                    .handle_message(WireMessage::SecureScreen(WireSecureScreenPayload {
                        code: "SECURE_SCREEN_CLEARED".into(),
                        active: false,
                        reason: "tree_restored".into(),
                        active_package: "com.bank".into(),
                        timestamp_ms: 1340,
                    }))
                    .await;
                dom_bg
                    .handle_message(WireMessage::Snapshot(WireSnapshotPayload {
                        seq: 12,
                        timestamp_ms: 1350,
                        reason: "unlocked".into(),
                        active_package: "com.bank".into(),
                        active_window_id: 1,
                        node_count: vault_bg.len(),
                        nodes: vault_bg,
                        secure_screen_active: Some(false),
                    }))
                    .await;
            });
            Ok(())
        }
    }

    let engine = MacroExecutionEngine::new(
        Arc::clone(&dom),
        graph,
        Arc::new(BioExec { dom, vault }),
        hasher,
        MacroExecutionConfiguration {
            step_timeout_ms: 120,
            post_settle_quiescence_ms: 0,
            allow_hazardous_actions: true,
            secure_handoff_timeout_secs: 5.0,
            ..Default::default()
        },
        None,
    );
    let mut ev_rx = engine.subscribe_events();

    let report = engine.execute(&[edge], "61061JEA307110").await.unwrap();
    assert_eq!(report.final_hash, h_vault);
    assert!(report.recovered_steps.contains(&"secure_handoff".to_string()));

    let mut saw_bio = false;
    let mut saw_cleared = false;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            MacroExecutionEvent::AwaitingBiometrics { .. } => saw_bio = true,
            MacroExecutionEvent::BiometricsCleared => saw_cleared = true,
            _ => {}
        }
    }
    assert!(saw_bio && saw_cleared);
}

#[tokio::test]
async fn test_suppressed_frame_between_significance_and_settle_threshold_and_forward_skip() {
    let base = ["Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta", "Theta"];
    let prev_labels: Vec<&str> = std::iter::once("Alpha").chain(base).collect();
    let scrolled_labels: Vec<&str> = base.into_iter().chain(std::iter::once("Iota")).collect();

    let mk_list = |labels: &[&str], id_base: i64| -> Vec<ShadowUINode> {
        labels
            .iter()
            .enumerate()
            .map(|(i, txt)| {
                make_node(
                    id_base + i as i64,
                    i as i32,
                    "com.android.settings",
                    "Button",
                    "",
                    txt,
                    300,
                    200 + (i as i32) * 100,
                    true,
                )
            })
            .collect()
    };

    let prev_nodes = mk_list(&prev_labels, 10);
    let scrolled_nodes = mk_list(&scrolled_labels, 30);
    let dest_nodes = vec![make_node(
        90,
        0,
        "com.android.settings",
        "TextView",
        "",
        "Bluetooth",
        300,
        200,
        false,
    )];
    let h_dest = ScreenLayoutHasher::default()
        .fingerprint(&ShadowTreeIndex::new(
            3,
            300,
            "com.android.settings",
            1,
            dest_nodes.clone(),
        ))
        .layout_hash;

    let bridge = ShadowGraphBridge::with_default_config();
    bridge
        .graph_engine
        .process(&make_update(1, "com.android.settings", prev_nodes.clone()))
        .await;
    bridge
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Alpha", Some(300), Some(200)))
        .await;
    bridge
        .graph_engine
        .process(&make_update(2, "com.android.settings", dest_nodes))
        .await;
    bridge
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Back", Some(50), Some(100)))
        .await;
    bridge
        .graph_engine
        .process(&make_update(3, "com.android.settings", prev_nodes))
        .await;

    bridge
        .dom_state
        .handle_message(make_snapshot_msg(10, "com.android.settings", scrolled_nodes))
        .await;

    struct NoOpExec;
    #[async_trait]
    impl MacroActionExecutor for NoOpExec {
        async fn execute(
            &self,
            _action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            Ok(())
        }
    }

    let outcome = bridge
        .route_and_execute(
            "61061JEA307110",
            Arc::new(NoOpExec),
            RouteExecuteOptions {
                target_hash: Some(h_dest),
                step_timeout_ms: 400,
                post_settle_quiescence_ms: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(outcome, ShadowNavigationOutcome::NoRoute { .. }));
    let _ = BTreeSet::<String>::new();
}

#[tokio::test]
async fn test_macro_execution_forward_skip_live_selector_retargeting_and_pre_subscribe_race() {
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let hasher = ScreenLayoutHasher::default();

    let n0 = vec![make_node(1, 0, "com.settings", "Button", "id/open", "Open List", 200, 300, true)];
    let n1 = vec![make_node(2, 0, "com.settings", "Button", "id/s1", "Scroll 1", 200, 400, true)];
    let n2 = vec![make_node(3, 0, "com.settings", "Button", "id/s2", "Scroll 2", 200, 500, true)];
    let n3_recorded = vec![make_node(4, 0, "com.settings", "Button", "id/deep", "Deep Option", 300, 700, true)];
    // Live version has "Deep Option" shifted from (300,700) to (340,920)
    let n3_moved = vec![make_node(4, 0, "com.settings", "Button", "id/deep", "Deep Option", 340, 920, true)];
    let n4 = vec![make_node(5, 0, "com.settings", "TextView", "id/done", "Deep Target", 300, 400, false)];

    let h4 = hasher
        .fingerprint(&ShadowTreeIndex::new(5, 500, "com.settings", 1, n4.clone()))
        .layout_hash;

    bridge.graph_engine.process(&make_update(1, "com.settings", n0.clone())).await;
    let mut a0 = AttributedUIAction::tap("Open List", Some(200), Some(300));
    a0.selector_text = Some("Open List".into());
    bridge.graph_engine.record_pending_action(a0).await;
    bridge.graph_engine.process(&make_update(2, "com.settings", n1)).await;

    bridge.graph_engine.record_pending_action(AttributedUIAction::swipe("down1", 500, 1500, 500, 600, Some(250))).await;
    bridge.graph_engine.process(&make_update(3, "com.settings", n2)).await;

    bridge.graph_engine.record_pending_action(AttributedUIAction::swipe("down2", 500, 1500, 500, 600, Some(250))).await;
    bridge.graph_engine.process(&make_update(4, "com.settings", n3_recorded)).await;

    let mut a3 = AttributedUIAction::tap("Deep Option", Some(300), Some(700));
    a3.selector_resource_id = Some("id/deep".into());
    a3.selector_text = Some("Deep Option".into());
    bridge.graph_engine.record_pending_action(a3).await;
    bridge.graph_engine.process(&make_update(5, "com.settings", n4.clone())).await;

    bridge.dom_state.handle_message(make_snapshot_msg(10, "com.settings", n0)).await;

    type DispatchedActionLog = Vec<(String, Option<i32>, Option<i32>)>;

    struct SkipAndRetargetExec {
        bridge: Arc<ShadowGraphBridge>,
        n3_moved: Vec<ShadowUINode>,
        n4: Vec<ShadowUINode>,
        dispatched: Mutex<DispatchedActionLog>,
    }
    #[async_trait]
    impl MacroActionExecutor for SkipAndRetargetExec {
        async fn execute(
            &self,
            action: &AttributedUIAction,
            _serial: &str,
        ) -> Result<(), PathDeviationError> {
            let mut log = self.dispatched.lock().await;
            log.push((action.signature(), action.x, action.y));
            if log.len() == 1 {
                // App restores straight to screen 3 with moved button coords BEFORE execute returns!
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(11, "com.settings", self.n3_moved.clone()))
                    .await;
            } else {
                self.bridge
                    .dom_state
                    .handle_message(make_snapshot_msg(12, "com.settings", self.n4.clone()))
                    .await;
            }
            Ok(())
        }
    }

    let exec = Arc::new(SkipAndRetargetExec {
        bridge: Arc::clone(&bridge),
        n3_moved,
        n4,
        dispatched: Mutex::new(Vec::new()),
    });

    let outcome = bridge
        .route_and_execute(
            "61061JEA307110",
            exec.clone(),
            RouteExecuteOptions {
                target_hash: Some(h4),
                step_timeout_ms: 500,
                post_settle_quiescence_ms: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let ShadowNavigationOutcome::Arrived(report) = outcome else {
        panic!("Expected Arrived with forward-skip, got {outcome:?}");
    };
    assert_eq!(report.final_hash, h4);
    assert_eq!(report.steps.len(), 4);
    assert!(!report.steps[0].was_skipped);
    assert!(report.steps[1].was_skipped);
    assert!(report.steps[2].was_skipped);
    assert!(!report.steps[3].was_skipped);
    assert!(report.recovered_steps.iter().any(|s| s.starts_with("forward_skip:")));

    // Verify MCP Arrived payload includes step_index, step, and recovered_steps
    let formatted =
        DroidPathMcpServerCore::format_navigation_outcome(&ShadowNavigationOutcome::Arrived(report.clone()));
    let json_val: serde_json::Value = serde_json::from_str(&formatted.text).unwrap();
    assert_eq!(json_val["status"], "arrived");
    assert_eq!(json_val["path_summary"][0]["step_index"], 0);
    assert_eq!(json_val["path_summary"][0]["step"], 0);
    assert_eq!(json_val["path_summary"][1]["step_index"], 1);
    assert_eq!(json_val["path_summary"][1]["skipped"], true);
    assert!(json_val["recovered_steps"].as_array().is_some_and(|arr| !arr.is_empty()));

    let calls = exec.dispatched.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "tap:Open List");
    assert_eq!(calls[1], ("tap:Deep Option".to_string(), Some(340), Some(920)));
}

#[test]
fn test_shadow_tree_index_passive_learning_click_resolves_descendant_title() {
    let mut row = make_node(10, 0, "com.android.settings", "LinearLayout", "com.android.settings:id/row", "", 540, 400, true);
    row.bounds = "[0,300][1080,500]".into();

    let mut title = make_node(11, 1, "com.android.settings", "TextView", "android:id/title", "Display & touch", 300, 380, false);
    title.parent_id = Some(10);
    title.bounds = "[100,340][500,420]".into();
    title.depth = 2;

    let mut summary = make_node(12, 2, "com.android.settings", "TextView", "android:id/summary", "Dark theme, Night Light", 300, 450, false);
    summary.parent_id = Some(10);
    summary.bounds = "[100,425][700,480]".into();
    summary.depth = 2;

    let idx = ShadowTreeIndex::new(1, 100, "com.android.settings", 1, vec![row, title, summary]);

    // Click inside the clickable row container to the right of the TextView bounds (x=900, y=400)
    let action = idx.attributed_tap_action(900, 400, Utc::now());
    assert_eq!(action.target_description, "Display & touch");
    assert_eq!(action.selector_text.as_deref(), Some("Display & touch"));
    assert_eq!(action.selector_resource_id.as_deref(), Some("android:id/title"));
}
