use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use droidpath_core::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
fn make_node(
    id: i64,
    pkg: &str,
    class_name: &str,
    res_id: &str,
    text: &str,
    cx: i32,
    cy: i32,
    clickable: bool,
    scrollable: bool,
) -> ShadowUINode {
    let bounds = if scrollable {
        "[0,180][1080,2200]".to_string()
    } else {
        format!("[{},{},][{},{}]", (cx - 200).max(0), (cy - 50).max(0), cx + 200, cy + 50)
            .replace(",]", "]")
    };
    ShadowUINode {
        index: id as i32,
        text: text.to_string(),
        resource_id: res_id.to_string(),
        content_desc: String::new(),
        class_name: class_name.to_string(),
        clickable,
        scrollable,
        bounds,
        center_x: cx,
        center_y: cy,
        node_id: id,
        parent_id: None,
        window_id: 1,
        child_index: id as i32,
        depth: 1,
        package_name: pkg.to_string(),
        editable: false,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

async fn feed_screen(bridge: &ShadowGraphBridge, seq: u64, pkg: &str, nodes: Vec<ShadowUINode>) {
    bridge
        .dom_state
        .handle_message(WireMessage::Snapshot(WireSnapshotPayload {
            seq,
            timestamp_ms: (seq as i64) * 1000,
            reason: "test".to_string(),
            active_package: pkg.to_string(),
            active_window_id: 1,
            node_count: nodes.len(),
            nodes,
            secure_screen_active: Some(false),
        }))
        .await;
}

type ActionCallback = Arc<
    dyn Fn(AttributedUIAction) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

struct ScriptedExecutor {
    recorded: Arc<Mutex<Vec<AttributedUIAction>>>,
    on_action: ActionCallback,
}

#[async_trait]
impl MacroActionExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        action: &AttributedUIAction,
        _serial: &str,
    ) -> Result<(), PathDeviationError> {
        if action.kind == UIActionKind::Tap && (action.x.is_none() || action.y.is_none()) {
            return Err(PathDeviationError::ActionDispatchFailed {
                step_index: -1,
                action_signature: action.signature(),
                underlying_message: "Missing tap coordinates (x, y).".to_string(),
            });
        }
        self.recorded.lock().await.push(action.clone());
        (self.on_action)(action.clone()).await;
        Ok(())
    }
}

#[test]
fn test_seed_graph_zero_pii_non_hazardous_deterministic_and_disjoint_matching() {
    let seed = SeedGraphCatalog::android_settings_seed_graph();
    assert_eq!(seed.vertices.len(), 20);
    assert_eq!(seed.edges.len(), 19);
    assert!(seed.root_state_hash.is_some());

    // 1. Strict Zero-PII, coordinate-free, non-hazardous audit
    for v in &seed.vertices {
        assert!(v.is_seed, "Vertex {:?} must be marked is_seed", v.sample_interactive_labels);
        assert_eq!(v.visit_count, 0);
        assert_eq!(v.active_package, "com.android.settings");
        for lbl in &v.sample_interactive_labels {
            assert_eq!(ScreenLayoutHasher::redact_pii(lbl), *lbl, "PII trigger in label: {lbl}");
        }
        for alias in &v.semantic_aliases {
            assert_eq!(ScreenLayoutHasher::redact_pii(alias), *alias, "PII trigger in alias: {alias}");
        }
        for tok in &v.semantic_tokens {
            assert_eq!(ScreenLayoutHasher::redact_pii(tok), *tok, "PII trigger in token: {tok}");
        }
    }

    for e in &seed.edges {
        assert!(e.is_seed);
        assert!(!e.is_hazardous, "Seed edge {} must not be hazardous", e.action_signature);
        let act = e.action.as_ref().expect("seed edge must have semantic action");
        assert_eq!(act.kind, UIActionKind::Tap);
        assert!(act.x.is_none() && act.y.is_none(), "Seed edge {} must be coordinate-free", e.action_signature);
        assert!(act.selector_text.is_some());
        let haz = HazardousActionClassifier::assess(Some(act), &e.action_signature, None, None);
        assert!(!haz.is_hazardous, "Seed action {} triggered hazard classifier", e.action_signature);
    }

    // 2. Pairwise disjointness: every seed vertex matches ITSELF (>= 0.60) and NEVER falsely matches another seed vertex!
    for a in &seed.vertices {
        let fp_a = ScreenSemanticFingerprint {
            layout_hash: a.layout_hash,
            active_package: a.active_package.clone(),
            active_window_id: 1,
            tokens: a.semantic_tokens.clone(),
            sample_interactive_labels: a.sample_interactive_labels.clone(),
        };
        let self_score = SeedGraphCatalog::seed_match_score(&fp_a, a);
        assert!(
            self_score >= SeedGraphCatalog::SEED_MATCH_THRESHOLD,
            "Seed {:?} failed self-match: {self_score}",
            a.sample_interactive_labels.first()
        );
        for b in &seed.vertices {
            if a.layout_hash == b.layout_hash {
                continue;
            }
            let cross_score = SeedGraphCatalog::seed_match_score(&fp_a, b);
            assert!(
                cross_score < SeedGraphCatalog::SEED_MATCH_THRESHOLD,
                "Cross-screen false positive! Screen {:?} matched seed {:?} with score {cross_score}",
                a.sample_interactive_labels.first(),
                b.sample_interactive_labels.first()
            );
        }
    }

    // 3. Serialization round-trip preserves isSeed, semanticAliases, and selectorAliases cleanly
    let json_bytes = serde_json::to_vec(&seed).expect("serialize seed graph");
    let decoded: SpatialGraphSnapshot =
        serde_json::from_slice(&json_bytes).expect("deserialize seed graph");
    assert_eq!(decoded.vertices.len(), seed.vertices.len());
    assert_eq!(decoded.edges.len(), seed.edges.len());
    assert!(decoded.vertices.iter().all(|v| v.is_seed));
    assert!(decoded.edges.iter().all(|e| e.is_seed));
    assert!(decoded
        .vertices
        .iter()
        .any(|v| v.semantic_aliases.iter().any(|a| a == "Экран")));
    assert!(decoded
        .edges
        .iter()
        .any(|e| e
            .action
            .as_ref()
            .is_some_and(|a| a.selector_aliases.iter().any(|s| s == "Экран"))));
}

#[tokio::test]
async fn test_seed_graph_cross_oem_and_multilingual_navigation_with_hydration_and_real_reuse() {
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let report = bridge
        .graph_engine
        .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
        .await;
    assert_eq!(report.vertices_added, 20);
    assert_eq!(report.edges_added, 19);
    assert_eq!(bridge.graph_engine.seed_vertex_count().await, 20);
    assert_eq!(bridge.graph_engine.hydrated_vertex_count().await, 0);

    // Real Russian Android phone screens (completely different hashes, OEM class/resource_ids, localized labels!)
    let ru_settings_root = vec![
        make_node(1, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/search", "Поиск настроек", 540, 220, true, false),
        make_node(2, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_net", "Сеть и интернет", 540, 420, true, false),
        make_node(3, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_bt", "Подключённые устройства", 540, 620, true, false),
        make_node(4, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_apps", "Приложения", 540, 820, true, false),
        make_node(5, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_disp", "Экран", 540, 1120, true, false),
        make_node(6, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_sys", "Система", 540, 1420, true, false),
        make_node(7, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_about", "О телефоне", 540, 1620, true, false),
    ];
    let ru_display = vec![
        make_node(10, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/toolbar_title", "Экран", 240, 180, false, false),
        make_node(11, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_bright", "Яркость", 540, 460, true, false),
        make_node(12, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_dark", "Тёмная тема", 540, 680, true, false),
        make_node(13, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_night", "Ночная подсветка", 540, 880, true, false),
        make_node(14, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_timeout", "Тайм-аут экрана", 540, 1080, true, false),
    ];
    let ru_night_light = vec![
        make_node(20, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/toolbar_title", "Ночная подсветка", 300, 180, false, false),
        make_node(21, "com.android.settings", "android.widget.Switch", "com.oem.settings:id/switch_nl", "Включить ночную подсветку", 540, 440, true, false),
        make_node(22, "com.android.settings", "android.widget.TextView", "com.oem.settings:id/row_sched", "Расписание", 540, 660, true, false),
        make_node(23, "com.android.settings", "android.widget.SeekBar", "com.oem.settings:id/seek_int", "Интенсивность", 540, 900, true, false),
    ];

    feed_screen(&bridge, 1, "com.android.settings", ru_settings_root.clone()).await;
    let real_root_hash = bridge
        .hasher
        .fingerprint(&bridge.dom_state.current_index().await.unwrap())
        .layout_hash;

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let b_cb = Arc::clone(&bridge);
    let disp_nodes = ru_display.clone();
    let nl_nodes = ru_night_light.clone();
    let seq_counter = Arc::new(std::sync::atomic::AtomicU64::new(10));

    let executor = Arc::new(ScriptedExecutor {
        recorded: Arc::clone(&recorded),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b_cb);
            let d = disp_nodes.clone();
            let nl = nl_nodes.clone();
            let sc = Arc::clone(&seq_counter);
            Box::pin(async move {
                let seq = sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if act.y == Some(1120) {
                    feed_screen(&b, seq, "com.android.settings", d).await;
                } else if act.y == Some(880) {
                    feed_screen(&b, seq, "com.android.settings", nl).await;
                }
            })
        }),
    });

    // Run 1: English target "Night Light" on a brand-new Russian device via Seed Graph!
    let outcome1 = bridge
        .route_and_execute(
            "TEST_SERIAL",
            Arc::clone(&executor) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Night Light".to_string()),
                max_steps: 6,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Seed route must succeed");

    let report1 = match outcome1 {
        ShadowNavigationOutcome::Arrived(r) => r,
        other => panic!("Expected Arrived on Run 1, got {other:?}"),
    };
    assert_eq!(report1.steps.len(), 2);
    assert!(
        report1
            .recovered_steps
            .iter()
            .any(|s| s.starts_with("seed_hydrated:")),
        "Expected seed hydration telemetry in {:?}",
        report1.recovered_steps
    );

    // Verify exact coordinates were resolved dynamically from Russian UI nodes
    let actions1 = recorded.lock().await.clone();
    assert_eq!(actions1.len(), 2);
    assert_eq!((actions1[0].x, actions1[0].y), (Some(540), Some(1120)));
    assert_eq!((actions1[1].x, actions1[1].y), (Some(540), Some(880)));

    // Verify Seed-to-Real Hydration inside SpatialGraphEngine!
    assert_eq!(bridge.graph_engine.hydrated_vertex_count().await, 3);
    assert_eq!(bridge.graph_engine.seed_vertex_count().await, 17);
    assert!(bridge.graph_engine.is_spanning_dag_acyclic().await);

    let root_v = bridge
        .graph_engine
        .vertex(real_root_hash)
        .await
        .expect("real root vertex hydrated");
    assert!(!root_v.is_seed);
    assert!(root_v.visit_count >= 1);
    assert!(root_v.semantic_aliases.iter().any(|a| a == "Настройки"));

    let root_edges = bridge.graph_engine.neighbors(real_root_hash).await;
    let calibrated_disp_edge = root_edges
        .iter()
        .find(|e| !e.is_seed && e.action.as_ref().and_then(|a| a.y) == Some(1120))
        .expect("calibrated real edge to Display must exist");
    assert_eq!(
        calibrated_disp_edge
            .action
            .as_ref()
            .and_then(|a| a.selector_text.as_deref()),
        Some("Экран")
    );
    // Unvisited seed branches (e.g. System, Battery, Apps) must have been rewired onto real_root_hash!
    assert!(
        root_edges.iter().any(|e| e.is_seed
            && e.action
                .as_ref()
                .and_then(|a| a.selector_text.as_deref())
                == Some("System")),
        "Remaining unvisited seed branches must stay wired to hydrated real root"
    );

    // Run 2: Return to Russian Settings Root and navigate to "Ночная подсветка" -> must use 100% REAL calibrated edges!
    recorded.lock().await.clear();
    feed_screen(&bridge, 50, "com.android.settings", ru_settings_root).await;
    let outcome2 = bridge
        .route_and_execute(
            "TEST_SERIAL",
            executor as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Ночная подсветка".to_string()),
                max_steps: 6,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Hydrated real route must succeed");

    let report2 = match outcome2 {
        ShadowNavigationOutcome::Arrived(r) => r,
        other => panic!("Expected Arrived on Run 2, got {other:?}"),
    };
    assert_eq!(report2.steps.len(), 2);
    assert!(
        report2.steps.iter().all(|s| s.matched_by_exact_hash),
        "Run 2 must settle by exact device hash: {:?}",
        report2.steps
    );
    assert!(
        !report2
            .recovered_steps
            .iter()
            .any(|s| s.starts_with("seed_hydrated:")),
        "Run 2 must use already-hydrated real edges without re-hydrating: {:?}",
        report2.recovered_steps
    );
}

#[tokio::test]
async fn test_seed_graph_cross_package_launch_app_entry_from_home_with_smart_scroll_and_hydration() {
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let _ = bridge
        .graph_engine
        .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
        .await;

    let home_nodes = vec![
        make_node(1, "com.google.android.apps.nexuslauncher", "android.widget.TextView", "com.launcher:id/clock", "10:42", 540, 220, false, false),
        make_node(2, "com.google.android.apps.nexuslauncher", "android.widget.TextView", "com.launcher:id/chrome", "Chrome", 240, 1800, true, false),
    ];
    // Settings Root viewport BEFORE scrolling: "System" is below the fold!
    let settings_top_nodes = vec![
        make_node(10, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "com.android.settings:id/recycler_view", "", 540, 1200, false, true),
        make_node(11, "com.android.settings", "android.widget.TextView", "android:id/title", "Search settings", 540, 260, true, false),
        make_node(12, "com.android.settings", "android.widget.TextView", "android:id/title", "Network & internet", 540, 480, true, false),
        make_node(13, "com.android.settings", "android.widget.TextView", "android:id/title", "Connected devices", 540, 700, true, false),
        make_node(14, "com.android.settings", "android.widget.TextView", "android:id/title", "Apps", 540, 920, true, false),
        make_node(15, "com.android.settings", "android.widget.TextView", "android:id/title", "Battery", 540, 1140, true, false),
    ];
    // Settings Root viewport AFTER scrolling down: "System" is revealed!
    let settings_bottom_nodes = vec![
        make_node(20, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "com.android.settings:id/recycler_view", "", 540, 1200, false, true),
        make_node(21, "com.android.settings", "android.widget.TextView", "android:id/title", "Location", 540, 500, true, false),
        make_node(22, "com.android.settings", "android.widget.TextView", "android:id/title", "Accessibility", 540, 740, true, false),
        make_node(23, "com.android.settings", "android.widget.TextView", "android:id/title", "System", 540, 1020, true, false),
        make_node(24, "com.android.settings", "android.widget.TextView", "android:id/title", "About phone", 540, 1260, true, false),
    ];
    let system_nodes = vec![
        make_node(30, "com.android.settings", "android.widget.TextView", "", "System", 240, 180, false, false),
        make_node(31, "com.android.settings", "android.widget.TextView", "android:id/title", "Languages", 540, 480, true, false),
        make_node(32, "com.android.settings", "android.widget.TextView", "android:id/title", "Date & time", 540, 720, true, false),
        make_node(33, "com.android.settings", "android.widget.TextView", "android:id/title", "Developer options", 540, 1060, true, false),
    ];
    let dev_options_nodes = vec![
        make_node(40, "com.android.settings", "android.widget.TextView", "", "Developer options", 300, 180, false, false),
        make_node(41, "com.android.settings", "android.widget.Switch", "android:id/title", "Use developer options", 540, 420, true, false),
        make_node(42, "com.android.settings", "android.widget.TextView", "android:id/title", "USB debugging", 540, 700, true, false),
        make_node(43, "com.android.settings", "android.widget.TextView", "android:id/title", "Show layout bounds", 540, 960, true, false),
    ];

    feed_screen(&bridge, 1, "com.google.android.apps.nexuslauncher", home_nodes).await;

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let b_cb = Arc::clone(&bridge);
    let seq_counter = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let s_top = settings_top_nodes.clone();
    let s_bot = settings_bottom_nodes.clone();
    let sys_n = system_nodes.clone();
    let dev_n = dev_options_nodes.clone();

    let executor = Arc::new(ScriptedExecutor {
        recorded: Arc::clone(&recorded),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b_cb);
            let sc = Arc::clone(&seq_counter);
            let s_top = s_top.clone();
            let s_bot = s_bot.clone();
            let sys_n = sys_n.clone();
            let dev_n = dev_n.clone();
            Box::pin(async move {
                let seq = sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match act.kind {
                    UIActionKind::LaunchApp if act.target_description == "com.android.settings" => {
                        feed_screen(&b, seq, "com.android.settings", s_top).await;
                    }
                    UIActionKind::Swipe => {
                        feed_screen(&b, seq, "com.android.settings", s_bot).await;
                    }
                    UIActionKind::Tap if act.y == Some(1020) => {
                        feed_screen(&b, seq, "com.android.settings", sys_n).await;
                    }
                    UIActionKind::Tap if act.y == Some(1060) => {
                        feed_screen(&b, seq, "com.android.settings", dev_n).await;
                    }
                    _ => {}
                }
            })
        }),
    });

    let outcome = bridge
        .route_and_execute(
            "TEST_SERIAL",
            executor as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Developer options".to_string()),
                max_steps: 8,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Cross-package Seed Graph route from Home must succeed");

    let report = match outcome {
        ShadowNavigationOutcome::Arrived(r) => r,
        other => panic!("Expected Arrived, got {other:?}"),
    };
    assert_eq!(report.steps.len(), 3); // LaunchApp -> System -> Developer options
    assert!(
        report
            .recovered_steps
            .iter()
            .any(|r| r == "smart_scroll_to_selector:down:1"),
        "Expected smart_scroll_to_selector to reveal off-screen System row: {:?}",
        report.recovered_steps
    );

    let actions = recorded.lock().await.clone();
    assert_eq!(actions[0].kind, UIActionKind::LaunchApp);
    assert_eq!(actions[0].target_description, "com.android.settings");
    assert_eq!(actions[1].kind, UIActionKind::Swipe);
    assert_eq!((actions[2].x, actions[2].y), (Some(540), Some(1020)));
    assert_eq!((actions[3].x, actions[3].y), (Some(540), Some(1060)));

    // 4 real vertices hydrated (Home + Settings + System + Developer options)
    assert_eq!(bridge.graph_engine.hydrated_vertex_count().await, 4);
    assert!(bridge.graph_engine.is_spanning_dag_acyclic().await);
}

#[tokio::test]
async fn test_seed_graph_idempotent_merge_manager_stats_and_loopback_cors() {
    let tmp_dir = std::env::temp_dir().join(format!("droidpath-seed-test-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
    let graph_path = tmp_dir.join("shadow_graph.json");

    let manager = DroidPathManager::new_with_seed(
        0,
        graph_path.clone(),
        None,
        DroidPathAdbRunner::default(),
        None,
        None,
        true,
    );
    let server = Arc::new(DroidPathMcpServerCore::new(Arc::clone(&manager)));

    // 1. Cold start loads Zero-PII Settings Seed Graph in memory without dirtying disk
    let stats_resp = server
        .handle_raw_request_data(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"android_graph_stats","arguments":{}}}"#,
        )
        .await
        .unwrap();
    let stats_json: serde_json::Value = serde_json::from_slice(&stats_resp).unwrap();
    let stats_inner: serde_json::Value =
        serde_json::from_str(stats_json["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(stats_inner["vertex_count"], 20);
    assert_eq!(stats_inner["seed_vertex_count"], 20);
    assert_eq!(stats_inner["hydrated_vertex_count"], 0);
    assert_eq!(stats_inner["edge_count"], 19);

    // Flushing untouched pure-seed graph must not write unsolicited file to disk
    manager.flush_persisted_graph().await;
    assert!(!graph_path.exists());

    // Re-merging seed graph is 100% idempotent
    let second_merge = manager
        .bridge
        .graph_engine
        .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
        .await;
    assert_eq!(second_merge.vertices_added, 0);
    assert_eq!(second_merge.edges_added, 0);

    // 2. Verify parameter alias compatibility (`key` for `keycode` in android_keyevent)
    let bad_key_resp = server
        .handle_raw_request_data(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"android_keyevent","arguments":{"key":"INVALID_KEY_XYZ"}}}"#,
        )
        .await
        .unwrap();
    let bad_key_str = String::from_utf8(bad_key_resp).unwrap();
    assert!(
        bad_key_str.contains("Unsupported or invalid keyevent 'INVALID_KEY_XYZ'"),
        "Expected key alias to be read: {bad_key_str}"
    );

    // 3. Verify loopback CORS preflight on DroidPathHttpServer (MCP Inspector compatibility)
    let http = DroidPathHttpServer::new(server);
    let port = http.start(0, None).await.unwrap();

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!(
                "OPTIONS /sse HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://localhost:6274\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    let preflight = String::from_utf8_lossy(&buf);
    assert!(preflight.contains("204 No Content"), "Got: {preflight}");
    assert!(
        preflight.contains("Access-Control-Allow-Origin: http://localhost:6274"),
        "Got: {preflight}"
    );
    assert!(preflight.contains("Vary: Origin"), "Got: {preflight}");

    // Evil origin must still be rejected with 403 Forbidden and NO CORS headers
    let mut evil_stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    evil_stream
        .write_all(
            format!(
                "OPTIONS /sse HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example.com\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut evil_buf = Vec::new();
    let _ = evil_stream.read_to_end(&mut evil_buf).await;
    let evil_resp = String::from_utf8_lossy(&evil_buf);
    assert!(evil_resp.contains("403 Forbidden"), "Got: {evil_resp}");
    assert!(
        !evil_resp.contains("Access-Control-Allow-Origin"),
        "Got: {evil_resp}"
    );

    http.stop().await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
}

#[test]
fn test_seed_graph_golden_fnv1a_hashes_for_swift_parity() {
    let seed = SeedGraphCatalog::android_settings_seed_graph();
    let root = seed.vertices.first().unwrap();
    let display = seed
        .vertices
        .iter()
        .find(|v| v.sample_interactive_labels.first().map(String::as_str) == Some("Display & touch"))
        .unwrap();
    let night_light = seed
        .vertices
        .iter()
        .find(|v| v.sample_interactive_labels.first().map(String::as_str) == Some("Night Light"))
        .unwrap();
    let dev_opts = seed
        .vertices
        .iter()
        .find(|v| v.sample_interactive_labels.first().map(String::as_str) == Some("Developer options"))
        .unwrap();

    // Ensure every vertex hash is unique, deterministic, and matches Swift AndroidShadowGraphKit bit-for-bit
    let unique_hashes: BTreeSet<u64> = seed.vertices.iter().map(|v| v.layout_hash).collect();
    assert_eq!(unique_hashes.len(), seed.vertices.len());
    assert_eq!(root.layout_hash, 17_917_625_146_408_918_530_u64);
    assert_eq!(display.layout_hash, 15_415_259_099_667_022_780_u64);
    assert_eq!(night_light.layout_hash, 10_514_516_610_508_614_137_u64);
    assert_eq!(dev_opts.layout_hash, 8_013_951_080_013_285_371_u64);

    // Deterministic re-invocation produces bit-exact identical snapshot
    let seed2 = SeedGraphCatalog::android_settings_seed_graph();
    assert_eq!(seed, seed2);
}

#[tokio::test]
async fn test_live_pump_hydration_zero_duplicates_smart_scroll_run2_post_hydration_chrome_and_http_injection() {
    // 1. Live graph_engine.process() race during Russian Seed-to-Real hydration:
    //    MUST NOT create duplicate edges or double-count visits/traversals!
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let _ = bridge
        .graph_engine
        .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
        .await;

    let ru_root = vec![
        make_node(1, "com.android.settings", "android.widget.TextView", "com.oem:id/s", "Поиск настроек", 540, 200, true, false),
        make_node(2, "com.android.settings", "android.widget.TextView", "com.oem:id/n", "Сеть и интернет", 540, 420, true, false),
        make_node(3, "com.android.settings", "android.widget.TextView", "com.oem:id/a", "Приложения", 540, 640, true, false),
        make_node(4, "com.android.settings", "android.widget.TextView", "com.oem:id/d", "Экран", 540, 1120, true, false),
    ];
    let ru_disp = vec![
        make_node(10, "com.android.settings", "android.widget.TextView", "com.oem:id/t", "Экран", 240, 180, false, false),
        make_node(11, "com.android.settings", "android.widget.TextView", "com.oem:id/b", "Яркость", 540, 440, true, false),
        make_node(12, "com.android.settings", "android.widget.TextView", "com.oem:id/nl", "Ночная подсветка", 540, 880, true, false),
    ];
    let ru_nl = vec![
        make_node(20, "com.android.settings", "android.widget.TextView", "com.oem:id/t", "Ночная подсветка", 240, 180, false, false),
        make_node(21, "com.android.settings", "android.widget.Switch", "com.oem:id/sw", "Включить ночную подсветку", 540, 440, true, false),
        make_node(22, "com.android.settings", "android.widget.TextView", "com.oem:id/sc", "Расписание", 540, 680, true, false),
    ];

    // Helper simulating ShadowGraphBridge::start() live pump calling graph_engine.process() on every DOM update
    async fn feed_with_live_pump(bridge: &ShadowGraphBridge, seq: u64, pkg: &str, nodes: Vec<ShadowUINode>) {
        feed_screen(bridge, seq, pkg, nodes).await;
        if let Some(idx) = bridge.dom_state.current_index().await {
            let upd = ShadowDOMUpdate {
                index: idx,
                is_full_snapshot: true,
                inserted_count: 1,
                updated_count: 0,
                removed_count: 0,
                event_type: None,
                event_package: None,
                is_secure_screen_active: false,
            };
            let _ = bridge.graph_engine.process(&upd).await;
        }
    }

    feed_with_live_pump(&bridge, 1, "com.android.settings", ru_root.clone()).await;
    let real_root_hash = bridge
        .hasher
        .fingerprint(&bridge.dom_state.current_index().await.unwrap())
        .layout_hash;

    let seq = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let b_cb = Arc::clone(&bridge);
    let ru_root_c = ru_root.clone();
    let ru_disp_c = ru_disp.clone();
    let ru_nl_c = ru_nl.clone();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(ScriptedExecutor {
        recorded: Arc::clone(&recorded),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b_cb);
            let sc = Arc::clone(&seq);
            let rr = ru_root_c.clone();
            let rd = ru_disp_c.clone();
            let rnl = ru_nl_c.clone();
            Box::pin(async move {
                let s = sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if act.kind == UIActionKind::LaunchApp && act.target_description == "com.android.settings" {
                    feed_with_live_pump(&b, s, "com.android.settings", rr).await;
                } else if act.y == Some(1120) {
                    feed_with_live_pump(&b, s, "com.android.settings", rd).await;
                } else if act.y == Some(880) {
                    feed_with_live_pump(&b, s, "com.android.settings", rnl).await;
                }
            })
        }),
    });

    let out = bridge
        .route_and_execute(
            "SER",
            Arc::clone(&exec) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Night Light".into()),
                max_steps: 6,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("live pump Russian hydration must succeed");
    assert!(matches!(out, ShadowNavigationOutcome::Arrived(_)));

    // Verify ZERO duplicate real edges from root -> display and exact traversal_count == 1
    let disp_hash = bridge
        .graph_engine
        .snapshot()
        .await
        .vertices
        .iter()
        .find(|v| !v.is_seed && v.sample_interactive_labels.iter().any(|l| l == "Яркость"))
        .unwrap()
        .layout_hash;
    let edges_to_disp: Vec<_> = bridge
        .graph_engine
        .neighbors(real_root_hash)
        .await
        .into_iter()
        .filter(|e| e.to_hash == disp_hash)
        .collect();
    assert_eq!(
        edges_to_disp.len(),
        1,
        "Live pump + hydrate_seed_transition MUST NOT create duplicate edges: {edges_to_disp:?}"
    );
    assert!(!edges_to_disp[0].is_seed);
    assert_eq!(edges_to_disp[0].traversal_count, 1);
    assert_eq!(
        edges_to_disp[0].action.as_ref().and_then(|a| a.selector_text.as_deref()),
        Some("Экран")
    );
    let disp_vertex = bridge.graph_engine.vertex(disp_hash).await.unwrap();
    assert_eq!(disp_vertex.visit_count, 1, "Must not double-count visit_count during live hydration");

    // 2. Post-hydration cross-app entry from Chrome -> ALREADY HYDRATED Night Light!
    let chrome_nodes = vec![
        make_node(99, "com.android.chrome", "android.widget.EditText", "com.android.chrome:id/url_bar", "example.com", 540, 140, true, false),
    ];
    feed_with_live_pump(&bridge, 100, "com.android.chrome", chrome_nodes).await;
    let from_chrome = bridge
        .route_and_execute(
            "SER",
            exec as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Night Light".into()),
                max_steps: 6,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Cross-app Settings entry must keep working AFTER hydration");
    match from_chrome {
        ShadowNavigationOutcome::Arrived(rep) => assert_eq!(rep.steps.len(), 3),
        other => panic!("Expected Arrived from Chrome to already-hydrated Night Light, got {other:?}"),
    }

    // 3. Smart-scroll 3x down on Run 1 + Run 2 MUST keep hydrated edge coordinate-free and re-scroll 3x without stale tap!
    let bridge2 = Arc::new(ShadowGraphBridge::with_default_config());
    let _ = bridge2
        .graph_engine
        .merge_seed_graph(SeedGraphCatalog::android_settings_seed_graph())
        .await;
    let s_p0 = vec![
        make_node(10, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "rv", "", 540, 1200, false, true),
        make_node(11, "com.android.settings", "android.widget.TextView", "android:id/title", "Search settings", 540, 260, true, false),
        make_node(12, "com.android.settings", "android.widget.TextView", "android:id/title", "Network & internet", 540, 500, true, false),
        make_node(13, "com.android.settings", "android.widget.TextView", "android:id/title", "Connected devices", 540, 740, true, false),
        make_node(14, "com.android.settings", "android.widget.TextView", "android:id/title", "Apps", 540, 1020, true, false),
    ];
    let s_p1 = vec![
        make_node(20, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "rv", "", 540, 1200, false, true),
        make_node(21, "com.android.settings", "android.widget.TextView", "android:id/title", "Notifications", 540, 400, true, false),
        make_node(22, "com.android.settings", "android.widget.TextView", "android:id/title", "Battery", 540, 700, true, false),
        make_node(23, "com.android.settings", "android.widget.TextView", "android:id/title", "Storage", 540, 1020, true, false),
    ];
    let s_p2 = vec![
        make_node(30, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "rv", "", 540, 1200, false, true),
        make_node(31, "com.android.settings", "android.widget.TextView", "android:id/title", "Sound & vibration", 540, 400, true, false),
        make_node(32, "com.android.settings", "android.widget.TextView", "android:id/title", "Display & touch", 540, 700, true, false),
        make_node(33, "com.android.settings", "android.widget.TextView", "android:id/title", "Security & privacy", 540, 1020, true, false),
    ];
    let s_p3 = vec![
        make_node(40, "com.android.settings", "androidx.recyclerview.widget.RecyclerView", "rv", "", 540, 1200, false, true),
        make_node(41, "com.android.settings", "android.widget.TextView", "android:id/title", "Location", 540, 400, true, false),
        make_node(42, "com.android.settings", "android.widget.TextView", "android:id/title", "Accessibility", 540, 700, true, false),
        make_node(43, "com.android.settings", "android.widget.TextView", "android:id/title", "System", 540, 1020, true, false),
    ];
    let sys_screen = vec![
        make_node(50, "com.android.settings", "android.widget.TextView", "", "System", 240, 180, false, false),
        make_node(51, "com.android.settings", "android.widget.TextView", "android:id/title", "Languages", 540, 480, true, false),
        make_node(52, "com.android.settings", "android.widget.TextView", "android:id/title", "Date & time", 540, 720, true, false),
        make_node(53, "com.android.settings", "android.widget.TextView", "android:id/title", "Developer options", 540, 1060, true, false),
    ];

    let scroll_page = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let scroll_page_cb = Arc::clone(&scroll_page);
    let seq2 = Arc::new(std::sync::atomic::AtomicU64::new(10));
    let b2_cb = Arc::clone(&bridge2);
    let pages = Arc::new(vec![s_p0.clone(), s_p1, s_p2, s_p3]);
    let sys_c = sys_screen.clone();
    let rec2 = Arc::new(Mutex::new(Vec::new()));
    let exec2 = Arc::new(ScriptedExecutor {
        recorded: Arc::clone(&rec2),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b2_cb);
            let pg = Arc::clone(&scroll_page_cb);
            let sc = Arc::clone(&seq2);
            let pages = Arc::clone(&pages);
            let sys_c = sys_c.clone();
            Box::pin(async move {
                let s = sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if act.kind == UIActionKind::Swipe {
                    let cur = pg.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    let idx = cur.min(pages.len() - 1);
                    feed_screen(&b, s, "com.android.settings", pages[idx].clone()).await;
                } else if act.kind == UIActionKind::Tap && act.y == Some(1020) && pg.load(std::sync::atomic::Ordering::SeqCst) == 3 {
                    // ONLY transition if we actually scrolled to page 3 (never on stale y=1020 from page 0/1/2!)
                    feed_screen(&b, s, "com.android.settings", sys_c).await;
                }
            })
        }),
    });

    // Run 1 (seed edge -> 3 scrolls down -> tap System)
    feed_screen(&bridge2, 1, "com.android.settings", s_p0.clone()).await;
    let p0_hash = bridge2.hasher.fingerprint(&bridge2.dom_state.current_index().await.unwrap()).layout_hash;
    let r1 = bridge2
        .route_and_execute(
            "SER",
            Arc::clone(&exec2) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("System".into()),
                max_steps: 4,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Run 1 with 3 scrolls down must succeed");
    assert!(matches!(r1, ShadowNavigationOutcome::Arrived(_)));

    // Verify hydrated edge from p0 -> System is coordinate-free (x=None, y=None)
    let hydrated_sys_edge = bridge2
        .graph_engine
        .neighbors(p0_hash)
        .await
        .into_iter()
        .find(|e| !e.is_seed)
        .expect("hydrated real edge to System");
    assert!(
        hydrated_sys_edge.action.as_ref().is_some_and(|a| a.x.is_none() && a.y.is_none()),
        "Smart-scrolled hydrated edge must remain coordinate-free: {:?}",
        hydrated_sys_edge.action
    );

    // Run 2 (hydrated real edge -> MUST still scroll 3 times down and arrive cleanly!)
    scroll_page.store(0, std::sync::atomic::Ordering::SeqCst);
    feed_screen(&bridge2, 50, "com.android.settings", s_p0).await;
    let r2 = bridge2
        .route_and_execute(
            "SER",
            exec2 as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("System".into()),
                max_steps: 4,
                step_timeout_ms: 1500,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("Run 2 on hydrated smart-scrolled edge must still scroll 3x and succeed");
    assert!(matches!(r2, ShadowNavigationOutcome::Arrived(_)));

    // 4. HTTP Security Hardening: Origin CRLF / path / userinfo injection + duplicate Host/Origin + conflicting Content-Length
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("http://localhost/evil\nSet-Cookie: pwned=1")));
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("http://localhost:6274/path")));
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("http://evil.com@localhost:6274")));
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("")));

    let tmp_dir = std::env::temp_dir().join(format!("droidpath-http-sec-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
    let mgr = DroidPathManager::new(
        0,
        tmp_dir.join("g.json"),
        None,
        DroidPathAdbRunner::default(),
        None,
        None,
    );
    let http = DroidPathHttpServer::new(Arc::new(DroidPathMcpServerCore::new(mgr)));
    let port = http.start(0, None).await.unwrap();

    async fn send_raw(port: u16, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    let crlf_resp = send_raw(
        port,
        &format!("OPTIONS /sse HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://localhost/x\nSet-Cookie: a=1\r\n\r\n"),
    )
    .await;
    assert!(crlf_resp.contains("400 Bad Request") || crlf_resp.contains("403 Forbidden"), "Got: {crlf_resp}");
    assert!(!crlf_resp.contains("Set-Cookie"), "Header injection must be blocked: {crlf_resp}");

    let dup_origin = send_raw(
        port,
        &format!("OPTIONS /sse HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.com\r\nOrigin: http://localhost:6274\r\n\r\n"),
    )
    .await;
    assert!(dup_origin.contains("403 Forbidden"), "Duplicate Origin must be rejected: {dup_origin}");

    let dup_cl = send_raw(
        port,
        &format!("POST /message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\n12345"),
    )
    .await;
    assert!(dup_cl.contains("400 Bad Request"), "Conflicting duplicate Content-Length must be 400: {dup_cl}");

    let plus_cl = send_raw(
        port,
        &format!("POST /message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: +5\r\n\r\n12345"),
    )
    .await;
    assert!(plus_cl.contains("400 Bad Request"), "Signed Content-Length '+5' must be 400: {plus_cl}");

    http.stop().await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
}
