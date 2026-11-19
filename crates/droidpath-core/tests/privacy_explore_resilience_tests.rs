use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use droidpath_core::{
    AStarCostConfiguration, AStarPathfinder, AttributedUIAction, DroidPathAdbRunner,
    DroidPathHttpServer, DroidPathManager, DroidPathMcpServerCore, ExploreOptions, ExploreOutcome,
    MacroActionExecutor, MacroExecutionConfiguration, MacroExecutionEngine, PathDeviationError,
    RouteExecuteOptions, ScreenLayoutHasher, ScreenStateVertex, ScreenTransitionEdge,
    ShadowDOMUpdate, ShadowGraphBridge, ShadowNavigationOutcome, ShadowTreeIndex, ShadowUINode,
    SpatialGraphEngine, SpatialGraphSnapshot, UIActionKind, WireMessage, WireSnapshotPayload,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
fn mk_node(
    id: i64,
    parent_id: Option<i64>,
    idx: i32,
    pkg: &str,
    cls: &str,
    rid: &str,
    text: &str,
    desc: &str,
    bounds: &str,
    clickable: bool,
    scrollable: bool,
    editable: bool,
) -> ShadowUINode {
    let rect = ShadowTreeIndex::parse_bounds_rect(bounds).unwrap_or(droidpath_core::BoundsRect {
        min_x: 0,
        min_y: 0,
        max_x: 200,
        max_y: 100,
    });
    ShadowUINode {
        node_id: id,
        parent_id,
        window_id: 1,
        index: idx,
        child_index: idx,
        depth: if parent_id.is_some() { 1 } else { 0 },
        package_name: pkg.to_string(),
        class_name: cls.to_string(),
        resource_id: rid.to_string(),
        text: text.to_string(),
        content_desc: desc.to_string(),
        bounds: bounds.to_string(),
        center_x: (rect.min_x + rect.max_x) / 2,
        center_y: (rect.min_y + rect.max_y) / 2,
        clickable,
        scrollable,
        editable,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

async fn push_screen(bridge: &ShadowGraphBridge, seq: u64, pkg: &str, nodes: Vec<ShadowUINode>) -> u64 {
    let node_count = nodes.len();
    let payload = WireSnapshotPayload {
        seq,
        timestamp_ms: 1_700_000_000_000 + (seq as i64) * 100,
        active_package: pkg.to_string(),
        active_window_id: 1,
        reason: "test".to_string(),
        node_count,
        nodes,
        secure_screen_active: Some(false),
    };
    bridge
        .dom_state
        .handle_message(WireMessage::Snapshot(payload))
        .await;
    let idx = bridge.dom_state.current_index().await.expect("index");
    bridge.hasher.fingerprint(&idx).layout_hash
}

async fn seed_screen_into_graph(
    bridge: &ShadowGraphBridge,
    seq: u64,
    pkg: &str,
    nodes: Vec<ShadowUINode>,
) -> u64 {
    let hash = push_screen(bridge, seq, pkg, nodes).await;
    let idx = bridge.dom_state.current_index().await.expect("index");
    let _ = bridge
        .graph_engine
        .process(&ShadowDOMUpdate {
            index: idx,
            is_full_snapshot: true,
            inserted_count: 1,
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: Some(pkg.to_string()),
            is_secure_screen_active: false,
        })
        .await;
    hash
}

type ScriptedHandler = Arc<
    dyn Fn(AttributedUIAction) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

struct ScriptedExecutor {
    handler: ScriptedHandler,
    log: Arc<Mutex<Vec<AttributedUIAction>>>,
}

impl ScriptedExecutor {
    fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(AttributedUIAction) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Self {
            handler: Arc::new(move |a| Box::pin(f(a))),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl MacroActionExecutor for ScriptedExecutor {
    async fn execute(&self, action: &AttributedUIAction, _serial: &str) -> Result<(), PathDeviationError> {
        self.log.lock().await.push(action.clone());
        (self.handler)(action.clone()).await;
        Ok(())
    }
}

#[tokio::test]
async fn test_privacy_redact_pii_luhn_otp_zero_leak_and_loopback_binding() {
    // 1. Valid Luhn card numbers (13-19 digits, spaced, dashed, contiguous) -> <CARD>
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Card 4532 0151 1283 0366"),
        "Card <CARD>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Pay with 4111-1111-1111-1111 now"),
        "Pay with <CARD> now"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Amex 378282246310005"),
        "Amex <CARD>"
    );

    // 2. Non-card numbers: bad Luhn, repeated digits, >19 digits -> untouched
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Not card 4111 1111 1111 1112"),
        "Not card 4111 1111 1111 1112"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Zeros 0000 0000 0000 0000"),
        "Zeros 0000 0000 0000 0000"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Ones 1111-1111-1111-1111"),
        "Ones 1111-1111-1111-1111"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Long 41111111111111110000"),
        "Long 41111111111111110000"
    );

    // 3. Context-anchored OTP codes (EN + RU, both marker orders) -> <OTP>
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Your verification code is 482910"),
        "Your verification code is <OTP>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("482910 is your verification code"),
        "<OTP> is your verification code"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Код подтверждения: 592041"),
        "Код подтверждения: <OTP>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("482910 — ваш код подтверждения"),
        "<OTP> — ваш код подтверждения"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("Код из СМС: 7391"),
        "Код из СМС: <OTP>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("2FA code 839201"),
        "2FA code <OTP>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("PIN: 0492"),
        "PIN: <OTP>"
    );
    assert_eq!(
        ScreenLayoutHasher::redact_pii("2026-05-12 Verification code: 482910"),
        "2026-05-12 Verification code: <OTP>"
    );

    // 4. Ordinary numbers untouched
    assert_eq!(ScreenLayoutHasher::redact_pii("Copyright 2026"), "Copyright 2026");
    assert_eq!(ScreenLayoutHasher::redact_pii("Display 1080x2400"), "Display 1080x2400");
    assert_eq!(ScreenLayoutHasher::redact_pii("shipping 1234"), "shipping 1234");
    assert_eq!(ScreenLayoutHasher::redact_pii("Order #482910"), "Order #482910");

    // 5. normalize_label runs redact_pii BEFORE collapsing digits into '#'
    assert_eq!(
        ScreenLayoutHasher::normalize_label("Card 4532 0151 1283 0366"),
        "Card <CARD>"
    );
    assert_eq!(
        ScreenLayoutHasher::normalize_label("Код подтверждения: 592041"),
        "Код подтверждения: <OTP>"
    );

    // 6. End-to-end SpatialGraphEngine persistence: zero leak of passwords, PAN, or OTP in shadow_graph.json
    let engine = SpatialGraphEngine::default();
    let screen_a = ShadowTreeIndex::new(
        1,
        1000,
        "com.bank.app",
        1,
        vec![
            mk_node(
                1,
                None,
                0,
                "com.bank.app",
                "EditText",
                "com.bank.app:id/password",
                "***",
                "Password",
                "[40,100][900,220]",
                true,
                false,
                true,
            ),
            mk_node(
                2,
                None,
                1,
                "com.bank.app",
                "TextView",
                "com.bank.app:id/card_number",
                "Card 4532 0151 1283 0366",
                "",
                "[40,260][900,360]",
                true,
                false,
                false,
            ),
            mk_node(
                3,
                None,
                2,
                "com.bank.app",
                "TextView",
                "com.bank.app:id/otp_msg",
                "Ваш код подтверждения: 482910",
                "",
                "[40,400][900,500]",
                true,
                false,
                false,
            ),
        ],
    );
    let _ = engine
        .process(&ShadowDOMUpdate {
            index: Arc::new(screen_a),
            is_full_snapshot: true,
            inserted_count: 3,
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: Some("com.bank.app".into()),
            is_secure_screen_active: false,
        })
        .await;

    let mut tap_with_pii = AttributedUIAction::tap("Card 4532 0151 1283 0366", Some(300), Some(310));
    tap_with_pii.selector_text = Some("Код подтверждения: 482910".to_string());
    tap_with_pii.text_input = Some("OTP 918273".to_string());
    engine.record_pending_action(tap_with_pii).await;

    let screen_b = ShadowTreeIndex::new(
        2,
        2000,
        "com.bank.app",
        1,
        vec![
            mk_node(
                10,
                None,
                0,
                "com.bank.app",
                "TextView",
                "com.bank.app:id/done",
                "Done",
                "",
                "[40,100][900,220]",
                true,
                false,
                false,
            ),
            mk_node(
                11,
                None,
                1,
                "com.bank.app",
                "TextView",
                "com.bank.app:id/year",
                "Copyright 2026",
                "",
                "[40,260][900,360]",
                false,
                false,
                false,
            ),
        ],
    );
    let edge = engine
        .process(&ShadowDOMUpdate {
            index: Arc::new(screen_b),
            is_full_snapshot: true,
            inserted_count: 2,
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: Some("com.bank.app".into()),
            is_secure_screen_active: false,
        })
        .await
        .expect("edge recorded");

    assert_eq!(edge.action_signature, "tap:Card <CARD>");
    let exported_bytes = engine.export_graph_json(true).await.expect("export json");
    let exported = String::from_utf8(exported_bytes).expect("utf8");
    let parsed_json: Value = serde_json::from_str(&exported).expect("valid exported json");

    // Recursively inspect every JSON string value (ignoring numeric FNV-1a layout hashes)
    fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::String(s) => out.push(s.as_str()),
            Value::Array(arr) => {
                for item in arr {
                    collect_strings(item, out);
                }
            }
            Value::Object(map) => {
                for val in map.values() {
                    collect_strings(val, out);
                }
            }
            _ => {}
        }
    }
    let mut all_strings = Vec::new();
    collect_strings(&parsed_json, &mut all_strings);
    let joined_strings = all_strings.join("\n");
    for forbidden in ["4532", "0151", "1283", "0366", "482910", "918273"] {
        assert!(
            !joined_strings.contains(forbidden),
            "PII fragment '{forbidden}' leaked into shadow_graph.json strings:\n{joined_strings}"
        );
    }
    assert!(joined_strings.contains("<CARD>"));
    assert!(joined_strings.contains("<OTP>"));
    assert!(joined_strings.contains("Copyright 2026"));

    // 7. Loopback binding (127.0.0.1) & non-loopback Host rejection on HTTP/SSE server
    let tmp_graph = std::env::temp_dir().join(format!("droidpath_loopback_{}.json", Uuid::new_v4()));
    let mgr = DroidPathManager::new(0, tmp_graph.clone(), None, DroidPathAdbRunner::default(), None, None);
    let core = Arc::new(DroidPathMcpServerCore::new(mgr));
    let http = DroidPathHttpServer::new(core);
    let port = http.start(0, None).await.expect("start loopback http");
    let bound_addr = http.bound_addr().await.expect("bound_addr");
    assert!(bound_addr.ip().is_loopback(), "HTTP server must bind strictly to loopback");
    assert_eq!(bound_addr.ip().to_string(), "127.0.0.1");

    // Verify non-loopback Host header is rejected with 403 Forbidden
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 192.168.1.42:8766\r\n\r\n")
        .await
        .expect("write");
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp).await;
    let resp_text = String::from_utf8_lossy(&resp);
    assert!(resp_text.starts_with("HTTP/1.1 403"), "Expected 403 for LAN Host, got: {resp_text}");
    http.stop().await;
    let _ = std::fs::remove_file(&tmp_graph);
}

#[tokio::test]
async fn test_explore_and_click_cold_start_scroll_reverse_prefix_hazard_needs_selection_and_auto_explore() {
    // --- Case A: Cold-start empty graph -> finds Display, taps, settles, records DAG edge ---
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let home_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "LinearLayout", "com.android.settings:id/row1", "", "", "[0,200][1080,360]", true, false, false),
        mk_node(2, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Display", "", "[80,240][500,320]", false, false, false),
        mk_node(3, None, 2, "com.android.settings", "TextView", "android:id/title", "Battery", "", "[80,400][500,480]", true, false, false),
    ];
    let display_nodes = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "", "Display", "", "[80,120][500,200]", false, false, false),
        mk_node(11, None, 1, "com.android.settings", "TextView", "com.android.settings:id/night_light", "Night Light", "", "[80,300][500,380]", true, false, false),
        mk_node(12, None, 2, "com.android.settings", "TextView", "com.android.settings:id/dark_theme", "Dark theme", "", "[80,440][500,520]", true, false, false),
        mk_node(13, None, 3, "com.android.settings", "TextView", "com.android.settings:id/pay_btn", "Pay Now", "", "[80,600][500,680]", true, false, false),
    ];
    push_screen(&bridge, 1, "com.android.settings", home_nodes.clone()).await;
    assert_eq!(bridge.graph_engine.vertex_count().await, 0);

    let b_clone = Arc::clone(&bridge);
    let disp_clone = display_nodes.clone();
    let exec_cold = Arc::new(ScriptedExecutor::new(move |_act| {
        let b = Arc::clone(&b_clone);
        let nodes = disp_clone.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let _ = push_screen(&b, 2, "com.android.settings", nodes).await;
        }
    }));

    let res = bridge
        .explore_and_click(
            "Display",
            "SER1",
            exec_cold,
            ExploreOptions {
                post_settle_quiescence_ms: 0,
                step_timeout_ms: 800,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("explore cold start");

    match res {
        ExploreOutcome::ExploredAndClicked {
            matched_candidate,
            scrolls_performed,
            screen_changed,
            edge_recorded,
            ..
        } => {
            assert_eq!(matched_candidate.label, "Display");
            assert_eq!(matched_candidate.score, 1100); // exact 1000 + clickable ancestor 100
            assert!(matched_candidate.clickable);
            assert_eq!(scrolls_performed, 0);
            assert!(screen_changed);
            assert!(edge_recorded);
        }
        other => panic!("Expected ExploredAndClicked, got {other:?}"),
    }
    assert_eq!(bridge.graph_engine.vertex_count().await, 2);
    assert_eq!(bridge.graph_engine.edge_count().await, 1);

    // --- Case B: Hazardous action guard in explore_and_click ---
    let noop_exec = Arc::new(ScriptedExecutor::new(|_| async {}));
    let haz_res = bridge
        .explore_and_click(
            "Pay Now",
            "SER1",
            Arc::clone(&noop_exec) as Arc<dyn MacroActionExecutor>,
            ExploreOptions {
                post_settle_quiescence_ms: 0,
                confirmed: false,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("hazard check");
    assert!(matches!(haz_res, ExploreOutcome::RequiresConfirmation(_)));
    assert!(noop_exec.log.lock().await.is_empty(), "Must not tap hazardous node without confirmed=true");

    // --- Case C: Scroll down hits bottom (unchanged), reverses UP, reveals target & records edges ---
    let bridge_scroll = Arc::new(ShadowGraphBridge::with_default_config());
    let bottom_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/list", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(2, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "System", "", "[80,300][600,400]", true, false, false),
        mk_node(3, Some(1), 2, "com.android.settings", "TextView", "android:id/title", "About phone", "", "[80,450][600,550]", true, false, false),
    ];
    let top_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/list", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(4, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Network & internet", "", "[80,300][600,400]", true, false, false),
        mk_node(5, Some(1), 2, "com.android.settings", "TextView", "android:id/title", "Connected devices", "", "[80,450][600,550]", true, false, false),
    ];
    let net_screen = vec![
        mk_node(20, None, 0, "com.android.settings", "TextView", "", "Network & internet", "", "[80,120][600,200]", false, false, false),
        mk_node(21, None, 1, "com.android.settings", "TextView", "com.android.settings:id/wifi", "Internet", "", "[80,300][600,400]", true, false, false),
    ];
    push_screen(&bridge_scroll, 1, "com.android.settings", bottom_nodes).await;

    let bs = Arc::clone(&bridge_scroll);
    let top_c = top_nodes.clone();
    let net_c = net_screen.clone();
    let seq_ctr = Arc::new(std::sync::atomic::AtomicU64::new(2));
    let exec_scroll = Arc::new(ScriptedExecutor::new(move |act| {
        let b = Arc::clone(&bs);
        let top = top_c.clone();
        let net = net_c.clone();
        let seq = Arc::clone(&seq_ctr);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            match act.kind {
                UIActionKind::Swipe => {
                    // Swipe DOWN (finger moves up: start_y > end_y) -> at bottom, no change!
                    // Swipe UP (finger moves down: start_y < end_y) -> reveals top_nodes!
                    if act.y.unwrap_or(0) < act.end_y.unwrap_or(0) {
                        let s = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = push_screen(&b, s, "com.android.settings", top).await;
                    }
                }
                UIActionKind::Tap => {
                    let s = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = push_screen(&b, s, "com.android.settings", net).await;
                }
                _ => {}
            }
        }
    }));

    let scroll_res = bridge_scroll
        .explore_and_click(
            "Network & internet",
            "SER1",
            Arc::clone(&exec_scroll) as Arc<dyn MacroActionExecutor>,
            ExploreOptions {
                max_scrolls: 3,
                step_timeout_ms: 200,
                post_settle_quiescence_ms: 0,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("scroll explore");

    match scroll_res {
        ExploreOutcome::ExploredAndClicked {
            matched_candidate,
            scrolls_performed,
            screen_changed,
            edge_recorded,
            ..
        } => {
            assert_eq!(matched_candidate.label, "Network & internet");
            assert_eq!(scrolls_performed, 2); // 1st down hit bottom -> 2nd up revealed target
            assert!(screen_changed);
            assert!(edge_recorded);
        }
        other => panic!("Expected ExploredAndClicked after bottom-bounce scroll, got {other:?}"),
    }

    // --- Case D: Prefix graph route ("Settings > Night Light") ---
    let bridge_prefix = Arc::new(ShadowGraphBridge::with_default_config());
    let home_scr = vec![
        mk_node(1, None, 0, "com.launcher", "TextView", "com.launcher:id/settings", "Settings", "", "[100,400][300,500]", true, false, false),
        mk_node(2, None, 1, "com.launcher", "TextView", "com.launcher:id/clock", "Clock", "", "[400,400][600,500]", true, false, false),
    ];
    let settings_scr = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "", "Settings", "", "[80,100][500,180]", false, false, false),
        mk_node(11, None, 1, "com.android.settings", "TextView", "com.android.settings:id/nl", "Night Light", "", "[80,300][500,380]", true, false, false),
    ];
    let nl_scr = vec![
        mk_node(20, None, 0, "com.android.settings", "TextView", "", "Night Light", "", "[80,100][500,180]", false, false, false),
        mk_node(21, None, 1, "com.android.settings", "Switch", "com.android.settings:id/toggle", "Use Night Light", "", "[80,300][500,380]", true, false, false),
    ];
    seed_screen_into_graph(&bridge_prefix, 1, "com.launcher", home_scr.clone()).await;
    bridge_prefix
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Settings", Some(200), Some(450)))
        .await;
    seed_screen_into_graph(&bridge_prefix, 2, "com.android.settings", settings_scr.clone()).await;
    // Reset live screen back to Home
    push_screen(&bridge_prefix, 3, "com.launcher", home_scr).await;

    let bp = Arc::clone(&bridge_prefix);
    let set_c = settings_scr.clone();
    let nl_c = nl_scr.clone();
    let seq_p = Arc::new(std::sync::atomic::AtomicU64::new(4));
    let exec_prefix = Arc::new(ScriptedExecutor::new(move |act| {
        let b = Arc::clone(&bp);
        let sc = set_c.clone();
        let nc = nl_c.clone();
        let seq = Arc::clone(&seq_p);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let s = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if act.target_description.contains("Settings") {
                let _ = push_screen(&b, s, "com.android.settings", sc).await;
            } else {
                let _ = push_screen(&b, s, "com.android.settings", nc).await;
            }
        }
    }));

    let prefix_res = bridge_prefix
        .explore_and_click(
            "Settings > Night Light",
            "SER1",
            exec_prefix,
            ExploreOptions {
                post_settle_quiescence_ms: 0,
                step_timeout_ms: 800,
                prefer_graph_route: true,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("prefix explore");
    match prefix_res {
        ExploreOutcome::ExploredAndClicked {
            matched_candidate,
            screen_changed,
            edge_recorded,
            ..
        } => {
            assert_eq!(matched_candidate.label, "Night Light");
            assert!(screen_changed);
            assert!(edge_recorded);
        }
        other => panic!("Expected prefix route + explore click, got {other:?}"),
    }

    // --- Case E: NeedsSelection returns <= 20 PII-scrubbed candidates & MCP isError: false ---
    let bridge_sel = Arc::new(ShadowGraphBridge::with_default_config());
    let mut many_nodes = Vec::new();
    for i in 0..25i32 {
        let label = if i == 0 {
            "Card 4532 0151 1283 0366".to_string()
        } else if i == 1 {
            "Code: 482910".to_string()
        } else {
            format!("Menu Item {i}")
        };
        many_nodes.push(mk_node(
            i64::from(i + 1),
            None,
            i,
            "com.example.shop",
            "TextView",
            &format!("com.example.shop:id/item_{i}"),
            &label,
            "",
            &format!("[40,{}][900,{}]", 100 + i * 60, 150 + i * 60),
            true,
            false,
            false,
        ));
    }
    push_screen(&bridge_sel, 1, "com.example.shop", many_nodes).await;
    let sel_res = bridge_sel
        .explore_and_click(
            "CompletelyMissingButton",
            "SER1",
            Arc::new(ScriptedExecutor::new(|_| async {})),
            ExploreOptions {
                max_scrolls: 0,
                post_settle_quiescence_ms: 0,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("needs selection");

    match &sel_res {
        ExploreOutcome::NeedsSelection { candidates, .. } => {
            assert_eq!(candidates.len(), 20, "Must cap candidates at top 20");
            assert!(candidates.iter().all(|c| !c.label.contains("4532") && !c.label.contains("482910")));
            assert!(candidates.iter().any(|c| c.label.contains("<CARD>")));
            assert!(candidates.iter().any(|c| c.label.contains("<OTP>")));
        }
        other => panic!("Expected NeedsSelection, got {other:?}"),
    }
    let mcp_sel = DroidPathMcpServerCore::format_explore_outcome(&sel_res);
    assert!(!mcp_sel.is_error, "needs_selection MUST have isError: false");
    let mcp_json: Value = serde_json::from_str(&mcp_sel.text).expect("valid json");
    assert_eq!(mcp_json["status"], "needs_selection");
    assert_eq!(mcp_json["candidate_count"], 20);

    // --- Case F: route_and_execute auto_explore flag ---
    let bridge_auto = Arc::new(ShadowGraphBridge::with_default_config());
    push_screen(&bridge_auto, 1, "com.android.settings", home_nodes).await;
    let ba = Arc::clone(&bridge_auto);
    let disp_c = display_nodes;
    let exec_auto = Arc::new(ScriptedExecutor::new(move |_| {
        let b = Arc::clone(&ba);
        let nodes = disp_c.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let _ = push_screen(&b, 2, "com.android.settings", nodes).await;
        }
    }));

    // auto_explore = false -> TargetNotFound (because Display screen isn't in graph; wait: home_nodes has "Display" as button on cold graph -> vertices empty -> TargetNotFound!)
    let no_auto = bridge_auto
        .route_and_execute(
            "SER1",
            Arc::clone(&exec_auto) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Battery".into()),
                auto_explore: false,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("route without auto_explore");
    // Home vertex was seeded with Battery in sample_interactive_labels -> AlreadyAtTarget or NoRoute/TargetNotFound.
    // Test with a target NOT on Home and NOT in graph:
    let _ = no_auto;
    let not_found = bridge_auto
        .route_and_execute(
            "SER1",
            Arc::clone(&exec_auto) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("NonexistentScreen".into()),
                auto_explore: false,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("target not found");
    assert!(matches!(not_found, ShadowNavigationOutcome::TargetNotFound { .. }));

    // With auto_explore = true on a target not in graph ("NonexistentScreen") -> returns Explored(NeedsSelection)
    let auto_sel = bridge_auto
        .route_and_execute(
            "SER1",
            Arc::clone(&exec_auto) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("NonexistentScreen".into()),
                auto_explore: true,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("auto_explore fallback");
    assert!(matches!(
        auto_sel,
        ShadowNavigationOutcome::Explored(ExploreOutcome::NeedsSelection { .. })
    ));

    // Cold-start launcher ambiguity: Home has clickable "Settings" icon in sample_interactive_labels.
    // route_and_execute(target: "Settings", auto_explore: true) MUST click Settings instead of
    // returning false AlreadyAtTarget, and once inside com.android.settings MUST return AlreadyAtTarget.
    let bridge_cold = Arc::new(ShadowGraphBridge::with_default_config());
    let launcher_nodes = vec![
        mk_node(1, None, 0, "com.google.android.apps.nexuslauncher", "TextView", "com.google.android.apps.nexuslauncher:id/icon", "Settings", "Settings", "[100,900][240,1100]", true, false, false),
        mk_node(2, None, 1, "com.google.android.apps.nexuslauncher", "TextView", "com.google.android.apps.nexuslauncher:id/icon", "Clock", "Clock", "[300,900][440,1100]", true, false, false),
    ];
    let settings_root_nodes = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "", "Settings", "", "[80,100][500,180]", false, false, false),
        mk_node(11, None, 1, "com.android.settings", "TextView", "android:id/title", "Network & internet", "", "[80,300][600,380]", true, false, false),
    ];
    push_screen(&bridge_cold, 1, "com.google.android.apps.nexuslauncher", launcher_nodes).await;
    let bc = Arc::clone(&bridge_cold);
    let sr = settings_root_nodes.clone();
    let exec_cold = Arc::new(ScriptedExecutor::new(move |_| {
        let b = Arc::clone(&bc);
        let nodes = sr.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let _ = push_screen(&b, 2, "com.android.settings", nodes).await;
        }
    }));

    let cold_res = bridge_cold
        .route_and_execute(
            "SER1",
            Arc::clone(&exec_cold) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Settings".into()),
                auto_explore: true,
                post_settle_quiescence_ms: 0,
                step_timeout_ms: 800,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("cold-start auto_explore");
    match cold_res {
        ShadowNavigationOutcome::Explored(ExploreOutcome::ExploredAndClicked {
            matched_candidate,
            screen_changed,
            edge_recorded,
            active_package,
            ..
        }) => {
            assert_eq!(matched_candidate.label, "Settings");
            assert!(screen_changed);
            assert!(edge_recorded);
            assert_eq!(active_package, "com.android.settings");
        }
        other => panic!("Expected cold-start auto_explore to click Settings, got {other:?}"),
    }

    // Calling again once actually inside com.android.settings must return AlreadyAtTarget (no tap).
    let noop_after = Arc::new(ScriptedExecutor::new(|_| async {}));
    let already_in = bridge_cold
        .route_and_execute(
            "SER1",
            Arc::clone(&noop_after) as Arc<dyn MacroActionExecutor>,
            RouteExecuteOptions {
                target_hint: Some("Settings".into()),
                auto_explore: true,
                post_settle_quiescence_ms: 0,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("already inside settings");
    assert!(matches!(
        already_in,
        ShadowNavigationOutcome::AlreadyAtTarget { ref active_package, .. } if active_package == "com.android.settings"
    ));
    assert!(noop_after.log.lock().await.is_empty());
}

#[tokio::test]
async fn test_router_resilience_back_to_root_smart_scroll_and_edge_decay() {
    // --- 1. App Task-Stack Drift (Back-to-Root) ---
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let home = vec![
        mk_node(1, None, 0, "com.launcher", "TextView", "com.launcher:id/s", "Settings", "", "[100,400][300,500]", true, false, false),
        mk_node(2, None, 1, "com.launcher", "TextView", "com.launcher:id/c", "Camera", "", "[400,400][600,500]", true, false, false),
    ];
    let settings_root = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "", "Settings", "", "[80,100][500,180]", false, false, false),
        mk_node(11, None, 1, "com.android.settings", "TextView", "com.android.settings:id/display", "Display", "", "[80,300][500,380]", true, false, false),
        mk_node(12, None, 2, "com.android.settings", "TextView", "com.android.settings:id/sound", "Sound", "", "[80,440][500,520]", true, false, false),
    ];
    let display_screen = vec![
        mk_node(20, None, 0, "com.android.settings", "TextView", "", "Display", "", "[80,100][500,180]", false, false, false),
        mk_node(21, None, 1, "com.android.settings", "TextView", "com.android.settings:id/br", "Brightness level", "", "[80,300][500,380]", true, false, false),
    ];
    let deep_sub_2 = vec![
        mk_node(30, None, 0, "com.android.settings", "TextView", "", "Legal information", "", "[80,100][700,180]", false, false, false),
        mk_node(31, None, 1, "com.android.settings", "TextView", "com.android.settings:id/lic", "Open source licenses", "", "[80,300][700,380]", true, false, false),
    ];
    let deep_sub_1 = vec![
        mk_node(40, None, 0, "com.android.settings", "TextView", "", "About phone", "", "[80,100][700,180]", false, false, false),
        mk_node(41, None, 1, "com.android.settings", "TextView", "com.android.settings:id/build", "Build number", "", "[80,300][700,380]", true, false, false),
    ];

    seed_screen_into_graph(&bridge, 1, "com.launcher", home.clone()).await;
    bridge
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Settings", Some(200), Some(450)))
        .await;
    seed_screen_into_graph(&bridge, 2, "com.android.settings", settings_root.clone()).await;
    bridge
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Display", Some(290), Some(340)))
        .await;
    seed_screen_into_graph(&bridge, 3, "com.android.settings", display_screen.clone()).await;
    // Also record a realistic `deep_sub_2 --(KEYCODE_HOME)--> home` edge (as happens whenever a user
    // leaves a deep subscreen via HOME). `replan_from` MUST NOT loop back through HOME -> Settings!
    seed_screen_into_graph(&bridge, 4, "com.android.settings", deep_sub_2.clone()).await;
    bridge
        .graph_engine
        .record_pending_action(AttributedUIAction::keyevent("HOME"))
        .await;
    seed_screen_into_graph(&bridge, 5, "com.launcher", home.clone()).await;
    // Start live device on Home
    push_screen(&bridge, 6, "com.launcher", home).await;

    let b_drift = Arc::clone(&bridge);
    let seq_d = Arc::new(std::sync::atomic::AtomicU64::new(7));
    let back_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bc_c = Arc::clone(&back_count);
    let sr_c = settings_root.clone();
    let ds_c = display_screen.clone();
    let d1_c = deep_sub_1.clone();
    let d2_c = deep_sub_2.clone();
    let exec_drift = Arc::new(ScriptedExecutor::new(move |act| {
        let b = Arc::clone(&b_drift);
        let seq = Arc::clone(&seq_d);
        let bc = Arc::clone(&bc_c);
        let sr = sr_c.clone();
        let ds = ds_c.clone();
        let d1 = d1_c.clone();
        let d2 = d2_c.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let s = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match act.kind {
                UIActionKind::Tap if act.target_description == "Settings" => {
                    // Simulate task-stack drift: Settings resumes on DeepSub2!
                    let _ = push_screen(&b, s, "com.android.settings", d2).await;
                }
                UIActionKind::Keyevent if act.target_description == "BACK" => {
                    let n = bc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if n == 1 {
                        let _ = push_screen(&b, s, "com.android.settings", d1).await;
                    } else {
                        let _ = push_screen(&b, s, "com.android.settings", sr).await;
                    }
                }
                UIActionKind::Tap if act.target_description == "Display" => {
                    let _ = push_screen(&b, s, "com.android.settings", ds).await;
                }
                _ => {}
            }
        }
    }));

    let nav_res = bridge
        .route_and_execute(
            "SER1",
            exec_drift,
            RouteExecuteOptions {
                target_hint: Some("Brightness level".into()),
                step_timeout_ms: 200,
                post_settle_quiescence_ms: 0,
                enable_auto_recovery: true,
                ..RouteExecuteOptions::default()
            },
        )
        .await
        .expect("back-to-root recovery");

    match nav_res {
        ShadowNavigationOutcome::Arrived(report) => {
            assert!(
                report.recovered_steps.iter().any(|s| s == "back_to_root:2"),
                "Expected back_to_root:2 in {:?}",
                report.recovered_steps
            );
        }
        other => panic!("Expected Arrived via back_to_root, got {other:?}"),
    }

    // --- 2. Smart Scroll-to-Selector inside MacroExecutionEngine ---
    let bridge_ss = Arc::new(ShadowGraphBridge::with_default_config());
    let list_top = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/rv", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(2, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Network", "", "[80,300][500,380]", true, false, false),
        mk_node(3, Some(1), 2, "com.android.settings", "TextView", "android:id/title", "Apps", "", "[80,440][500,520]", true, false, false),
    ];
    let list_bottom = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/rv", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(4, Some(1), 1, "com.android.settings", "TextView", "com.android.settings:id/about_id", "About phone", "", "[80,900][500,980]", true, false, false),
    ];
    let about_page = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "", "About phone", "", "[80,100][500,180]", false, false, false),
        mk_node(11, None, 1, "com.android.settings", "TextView", "com.android.settings:id/model", "Pixel 10a", "", "[80,300][500,380]", false, false, false),
    ];

    let start_h = seed_screen_into_graph(&bridge_ss, 1, "com.android.settings", list_top).await;
    let mut tap_about = AttributedUIAction::tap("About phone", Some(10), Some(10)); // stale coords!
    tap_about.selector_text = Some("About phone".into());
    tap_about.selector_resource_id = Some("com.android.settings:id/about_id".into());
    bridge_ss.graph_engine.record_pending_action(tap_about.clone()).await;
    let target_h = seed_screen_into_graph(&bridge_ss, 2, "com.android.settings", about_page.clone()).await;

    // Reset live screen to list_top (where About phone is off-screen)
    let list_top_again = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/rv", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(2, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Network", "", "[80,300][500,380]", true, false, false),
        mk_node(3, Some(1), 2, "com.android.settings", "TextView", "android:id/title", "Apps", "", "[80,440][500,520]", true, false, false),
    ];
    push_screen(&bridge_ss, 3, "com.android.settings", list_top_again).await;

    let bss = Arc::clone(&bridge_ss);
    let lb_c = list_bottom.clone();
    let ap_c = about_page.clone();
    let seq_ss = Arc::new(std::sync::atomic::AtomicU64::new(4));
    let exec_ss = Arc::new(ScriptedExecutor::new(move |act| {
        let b = Arc::clone(&bss);
        let lb = lb_c.clone();
        let ap = ap_c.clone();
        let seq = Arc::clone(&seq_ss);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            let s = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if act.kind == UIActionKind::Swipe {
                let _ = push_screen(&b, s, "com.android.settings", lb).await;
            } else if act.kind == UIActionKind::Tap && act.y == Some(940) {
                // Only transition if MacroExecutionEngine locked onto the real scrolled-into-view coords (y=940)!
                let _ = push_screen(&b, s, "com.android.settings", ap).await;
            }
        }
    }));

    let edges = bridge_ss.graph_engine.neighbors(start_h).await;
    assert_eq!(edges.len(), 1);
    let macro_engine = MacroExecutionEngine::new(
        Arc::clone(&bridge_ss.dom_state),
        Arc::clone(&bridge_ss.graph_engine),
        exec_ss,
        bridge_ss.hasher.clone(),
        MacroExecutionConfiguration {
            step_timeout_ms: 400,
            post_settle_quiescence_ms: 0,
            enable_smart_scroll_to_selector: true,
            ..MacroExecutionConfiguration::default()
        },
        None,
    );
    let ss_report = macro_engine.execute(&edges, "SER1").await.expect("smart scroll tap");
    assert_eq!(ss_report.final_hash, target_h);
    assert!(
        ss_report
            .recovered_steps
            .iter()
            .any(|r| r == "smart_scroll_to_selector:down:1"),
        "Expected smart_scroll_to_selector:down:1 in {:?}",
        ss_report.recovered_steps
    );

    // --- 3. Edge Decay & Self-Pruning (consecutiveFailures) ---
    // 3a. Backward-compatible JSON decode (missing consecutiveFailures -> 0)
    let legacy_json = json!({
        "rootStateHash": 1,
        "currentStateHash": 1,
        "vertices": [
            {
                "layoutHash": 1, "activePackage": "com.app", "sampleInteractiveLabels": ["A"],
                "semanticTokens": ["TextView||A"], "firstSeenAt": "2026-01-01T00:00:00Z",
                "lastSeenAt": "2026-01-01T00:00:00Z", "visitCount": 1, "discoveryDepth": 0
            },
            {
                "layoutHash": 2, "activePackage": "com.app", "sampleInteractiveLabels": ["B"],
                "semanticTokens": ["TextView||B"], "firstSeenAt": "2026-01-01T00:00:00Z",
                "lastSeenAt": "2026-01-01T00:00:00Z", "visitCount": 1, "discoveryParentHash": 1, "discoveryDepth": 1
            }
        ],
        "edges": [
            {
                "id": "11111111-1111-1111-1111-111111111111",
                "fromHash": 1, "toHash": 2,
                "action": { "kind": "tap", "targetDescription": "Go", "timestamp": "2026-01-01T00:00:00Z" },
                "actionSignature": "tap:Go", "traversalCount": 3, "averageTransitionMs": 100.0,
                "isSpanningTreeEdge": true, "lastTraversedAt": "2026-01-01T00:00:00Z"
            }
        ],
        "spanningChildren": [1, [2]]
    });
    let decay_engine = SpatialGraphEngine::default();
    decay_engine
        .import_graph_json(legacy_json.to_string().as_bytes())
        .await
        .expect("import legacy json without consecutiveFailures");
    let imported_edge = decay_engine.neighbors(1).await.pop().expect("edge");
    assert_eq!(imported_edge.consecutive_failures, 0);

    // 3b. A* cost penalty: cost *= 1.0 + 0.75 * consecutive_failures steers away from failing edge
    let now = Utc::now();
    let v = |h: u64, lbl: &str| ScreenStateVertex {
        layout_hash: h,
        active_package: "com.app".into(),
        sample_interactive_labels: vec![lbl.into()],
        semantic_tokens: BTreeSet::from([format!("TextView||{lbl}")]),
        first_seen_at: now,
        last_seen_at: now,
        visit_count: 1,
        discovery_parent_hash: None,
        discovery_depth: 0,
    };
    let e_direct_failing = ScreenTransitionEdge {
        id: Uuid::new_v4(),
        from_hash: 1,
        to_hash: 3,
        action: Some(AttributedUIAction::tap("Direct", Some(10), Some(10))),
        action_signature: "tap:Direct".into(),
        traversal_count: 3,
        average_transition_ms: 100.0,
        is_spanning_tree_edge: true,
        last_traversed_at: now,
        is_hazardous: false,
        hazard_reason: None,
        consecutive_failures: 2, // cost *= 1.0 + 1.5 = 2.5x (~2.51) > 2-hop healthy path (~2.01)!
    };
    let e_hop1 = ScreenTransitionEdge {
        id: Uuid::new_v4(),
        from_hash: 1,
        to_hash: 2,
        action: Some(AttributedUIAction::tap("Via", Some(20), Some(20))),
        action_signature: "tap:Via".into(),
        traversal_count: 3,
        average_transition_ms: 100.0,
        is_spanning_tree_edge: false,
        last_traversed_at: now,
        is_hazardous: false,
        hazard_reason: None,
        consecutive_failures: 0,
    };
    let e_hop2 = ScreenTransitionEdge {
        id: Uuid::new_v4(),
        from_hash: 2,
        to_hash: 3,
        action: Some(AttributedUIAction::tap("Finish", Some(30), Some(30))),
        action_signature: "tap:Finish".into(),
        traversal_count: 3,
        average_transition_ms: 100.0,
        is_spanning_tree_edge: false,
        last_traversed_at: now,
        is_hazardous: false,
        hazard_reason: None,
        consecutive_failures: 0,
    };
    let snap = SpatialGraphSnapshot {
        root_state_hash: Some(1),
        current_state_hash: Some(1),
        vertices: vec![v(1, "Start"), v(2, "Mid"), v(3, "Goal")],
        edges: vec![e_direct_failing, e_hop1, e_hop2],
        spanning_children: BTreeMap::new(),
    };
    let pf = AStarPathfinder::new(AStarCostConfiguration::default());
    let chosen = pf.find_shortest_path(1, 3, &snap).expect("path");
    assert_eq!(chosen.edges.len(), 2, "A* must prefer 2-hop healthy route over 2x-decayed direct edge");

    // 3c. 5 consecutive StepTimeouts via MacroExecutionEngine prune dead edge & preserve acyclic DAG
    let bridge_prune = Arc::new(ShadowGraphBridge::with_default_config());
    let s1 = vec![mk_node(1, None, 0, "com.app", "TextView", "id/go", "Go", "", "[10,10][200,100]", true, false, false)];
    let s2 = vec![mk_node(2, None, 0, "com.app", "TextView", "id/end", "End", "", "[10,10][200,100]", true, false, false)];
    let h1 = seed_screen_into_graph(&bridge_prune, 1, "com.app", s1.clone()).await;
    bridge_prune
        .graph_engine
        .record_pending_action(AttributedUIAction::tap("Go", Some(100), Some(50)))
        .await;
    let _h2 = seed_screen_into_graph(&bridge_prune, 2, "com.app", s2).await;
    push_screen(&bridge_prune, 3, "com.app", s1).await;

    let dead_edges = bridge_prune.graph_engine.neighbors(h1).await;
    assert_eq!(dead_edges.len(), 1);
    let dead_edge_id = dead_edges[0].id;

    let fail_engine = MacroExecutionEngine::new(
        Arc::clone(&bridge_prune.dom_state),
        Arc::clone(&bridge_prune.graph_engine),
        Arc::new(ScriptedExecutor::new(|_| async {})),
        bridge_prune.hasher.clone(),
        MacroExecutionConfiguration {
            step_timeout_ms: 60,
            post_settle_quiescence_ms: 0,
            ..MacroExecutionConfiguration::default()
        },
        None,
    );

    for expected_fail in 1..=4 {
        let err = fail_engine.execute(&dead_edges, "SER1").await.unwrap_err();
        assert!(matches!(err, PathDeviationError::StepTimeout { .. }));
        let cur_edges = bridge_prune.graph_engine.neighbors(h1).await;
        assert_eq!(cur_edges.len(), 1);
        assert_eq!(cur_edges[0].consecutive_failures, expected_fail);
    }

    // Verify reset on success works before pruning
    bridge_prune.graph_engine.record_edge_success(dead_edge_id).await;
    assert_eq!(bridge_prune.graph_engine.neighbors(h1).await[0].consecutive_failures, 0);

    // Now fail 5 consecutive times -> must auto-prune!
    for _ in 1..=5 {
        let err = fail_engine.execute(&dead_edges, "SER1").await.unwrap_err();
        assert!(matches!(err, PathDeviationError::StepTimeout { .. }));
    }
    assert!(
        bridge_prune.graph_engine.neighbors(h1).await.is_empty(),
        "Dead edge must be pruned at consecutive_failures >= 5"
    );
    assert!(bridge_prune.graph_engine.is_spanning_dag_acyclic().await);
}

#[tokio::test]
async fn test_mcp_tools_list_registers_explore_and_click_and_auto_explore() {
    let tmp_graph = std::env::temp_dir().join(format!("droidpath_mcp_list_{}.json", Uuid::new_v4()));
    let mgr = DroidPathManager::new(0, tmp_graph.clone(), None, DroidPathAdbRunner::default(), None, None);
    let core = DroidPathMcpServerCore::new(mgr);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    let resp_bytes = core
        .handle_raw_request_data(req.to_string().as_bytes())
        .await
        .expect("response");
    let resp: Value = serde_json::from_slice(&resp_bytes).expect("json");
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let nav_tool = tools
        .iter()
        .find(|t| t["name"] == "android_navigate_to_node")
        .expect("android_navigate_to_node registered");
    assert!(
        nav_tool["inputSchema"]["properties"]["auto_explore"].is_object(),
        "auto_explore parameter must be registered on android_navigate_to_node"
    );

    let explore_tool = tools
        .iter()
        .find(|t| t["name"] == "android_explore_and_click")
        .expect("android_explore_and_click registered");
    assert!(explore_tool["inputSchema"]["properties"]["target"].is_object());
    assert!(explore_tool["inputSchema"]["properties"]["max_scrolls"].is_object());
    assert!(explore_tool["inputSchema"]["properties"]["prefer_graph_route"].is_object());
    assert!(explore_tool["inputSchema"]["properties"]["confirmed"].is_object());

    // Verify score_live_candidates exact scoring contract directly
    let idx = ShadowTreeIndex::new(
        1,
        1000,
        "com.example",
        1,
        vec![
            mk_node(1, None, 0, "com.example", "TextView", "com.example:id/a", "Display", "", "[0,100][300,200]", true, false, false),
            mk_node(2, None, 1, "com.example", "TextView", "com.example:id/b", "Display settings", "", "[0,210][300,300]", false, false, false),
            mk_node(3, None, 2, "com.example", "TextView", "com.example:id/night_display", "Colors", "", "[0,310][300,400]", false, false, false),
            mk_node(4, None, 3, "com.example", "TextView", "com.example:id/d", "My Display", "", "[0,410][300,500]", false, false, false),
            mk_node(5, None, 4, "com.android.systemui", "TextView", "com.android.systemui:id/s", "Display", "", "[0,0][300,80]", true, false, false),
        ],
    );
    let mut ignored = HashSet::new();
    ignored.insert("com.android.systemui".to_string());
    let scored = ShadowGraphBridge::score_live_candidates(&idx, "Display", &ignored);
    assert_eq!(scored.len(), 4, "SystemUI node must be ignored");
    assert_eq!(scored[0].node_id, 1);
    assert_eq!(scored[0].score, 1100); // exact 1000 + clickable 100
    assert_eq!(scored[1].node_id, 2);
    assert_eq!(scored[1].score, 700); // prefix 700
    assert_eq!(scored[2].node_id, 3);
    assert_eq!(scored[2].score, 600); // resourceId suffix 600
    assert_eq!(scored[3].node_id, 4);
    assert_eq!(scored[3].score, 500); // contains 500

    let _ = std::fs::remove_file(&tmp_graph);
}

#[tokio::test]
async fn test_scroll_direct_edge_bridging_across_mid_scroll_frames_and_launcher_workspace_filter() {
    // 1. largest_scrollable_rect must ignore horizontal pagers & launcher home workspaces
    //    so explore_and_click never swipes down on Home screen and pulls down Notification Shade.
    let launcher_idx = ShadowTreeIndex::new(
        1,
        1000,
        "com.google.android.apps.nexuslauncher",
        1,
        vec![
            mk_node(1, None, 0, "com.google.android.apps.nexuslauncher", "ScrollView", "com.google.android.apps.nexuslauncher:id/workspace", "", "", "[0,0][1080,2424]", false, true, false),
            mk_node(2, None, 1, "com.google.android.apps.nexuslauncher", "ViewPager", "com.google.android.apps.nexuslauncher:id/smartspace_card_pager", "", "At a Glance", "[50,181][1030,432]", false, true, false),
            mk_node(3, None, 2, "com.google.android.apps.nexuslauncher", "HorizontalScrollView", "com.google.android.apps.nexuslauncher:id/carousel", "", "", "[0,500][1080,800]", false, true, false),
        ],
    );
    let mut ignored = HashSet::new();
    ignored.insert("com.android.systemui".to_string());
    assert!(
        MacroExecutionEngine::largest_scrollable_rect(&launcher_idx, &ignored).is_none(),
        "Launcher workspace, ViewPager, and HorizontalScrollView must not be vertically scrolled"
    );

    // 2. When mid-scroll deceleration frames arrive during explore_and_click scrolling and consume
    //    `pending_action`, a direct executable swipe edge (before_hash -> settled_after_hash) must
    //    still be recorded so A* can route end-to-end without hitting an `implicit` gap.
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let top_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/list", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(2, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Brightness", "", "[80,300][600,400]", true, false, false),
    ];
    let mid_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/list", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(3, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Lock screen", "", "[80,300][600,400]", true, false, false),
    ];
    let settled_nodes = vec![
        mk_node(1, None, 0, "com.android.settings", "RecyclerView", "com.android.settings:id/list", "", "", "[0,200][1080,2000]", false, true, false),
        mk_node(4, Some(1), 1, "com.android.settings", "TextView", "android:id/title", "Night Light", "", "[80,900][600,1000]", true, false, false),
    ];
    let goal_nodes = vec![
        mk_node(10, None, 0, "com.android.settings", "TextView", "android:id/title", "Use Night Light", "", "[80,300][600,400]", true, false, false),
    ];
    let h_top = seed_screen_into_graph(&bridge, 1, "com.android.settings", top_nodes).await;

    let b = Arc::clone(&bridge);
    let mid_c = mid_nodes.clone();
    let settled_c = settled_nodes.clone();
    let goal_c = goal_nodes.clone();
    let seq_ctr = Arc::new(std::sync::atomic::AtomicU64::new(2));
    let exec = Arc::new(ScriptedExecutor::new(move |act| {
        let b = Arc::clone(&b);
        let mid = mid_c.clone();
        let settled = settled_c.clone();
        let goal = goal_c.clone();
        let seq = Arc::clone(&seq_ctr);
        async move {
            match act.kind {
                UIActionKind::Swipe => {
                    // Simulate real Android fling: mid-scroll frame consumes pending_action first,
                    // then final settled frame arrives during quiescence.
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    let s1 = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = seed_screen_into_graph(&b, s1, "com.android.settings", mid).await;
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    let s2 = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = seed_screen_into_graph(&b, s2, "com.android.settings", settled).await;
                }
                UIActionKind::Tap => {
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    let s3 = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = seed_screen_into_graph(&b, s3, "com.android.settings", goal).await;
                }
                _ => {}
            }
        }
    }));

    let outcome = bridge
        .explore_and_click(
            "Night Light",
            "SER1",
            exec,
            ExploreOptions {
                max_scrolls: 2,
                step_timeout_ms: 400,
                post_settle_quiescence_ms: 80,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("explore scroll with mid-frame");

    let h_goal = match outcome {
        ExploreOutcome::ExploredAndClicked { final_hash, .. } => final_hash,
        other => panic!("Expected ExploredAndClicked, got {other:?}"),
    };

    let snap = bridge.graph_engine.snapshot().await;
    let pf = AStarPathfinder::new(AStarCostConfiguration::default());
    let mut goals = HashSet::new();
    goals.insert(h_goal);
    let route = pf
        .find_shortest_path_to_any(h_top, &goals, &snap)
        .expect("A* must find continuous executable route across bridged scroll edge");
    assert_eq!(route.edges.len(), 2, "Expected 2-hop route (scroll -> tap Night Light)");
    assert!(route.edges.iter().all(|e| e.action.is_some()));
}
