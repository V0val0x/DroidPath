use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use droidpath_core::{
    AttributedUIAction, DaemonClientError, DaemonCommand, DaemonEndpointConfiguration,
    DaemonTcpClient, DroidPathActionExecutor, DroidPathAdbRunner, ExploreOptions, ExploreOutcome,
    MacroActionExecutor, MacroExecutionConfiguration, MacroExecutionEngine, NdjsonWireCodec,
    PathDeviationError, ScreenLayoutHasher, ScreenTransitionEdge, ShadowGraphBridge, ShadowUINode,
    UIActionKind, WireCodec, WireMessage, WireSnapshotPayload,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

#[test]
fn codec_encodes_input_commands_and_decodes_capabilities_and_input_result() {
    let codec = NdjsonWireCodec::new();

    // Backward-compatible hello (no capabilities key)
    let legacy_hello = br#"{"type":"hello","protocol_version":1,"format":"ndjson/1","sdk_int":36,"device_model":"Pixel","pid":42}"#;
    match codec.decode_frame(legacy_hello).expect("legacy hello") {
        WireMessage::Hello(h) => assert!(!h.supports_native_input()),
        other => panic!("Expected Hello, got {other:?}"),
    }

    // New hello with input capability
    let new_hello = br#"{"type":"hello","protocol_version":1,"format":"ndjson/1","sdk_int":37,"device_model":"Pixel 10a","pid":42,"capabilities":["input"]}"#;
    match codec.decode_frame(new_hello).expect("new hello") {
        WireMessage::Hello(h) => assert!(h.supports_native_input()),
        other => panic!("Expected Hello, got {other:?}"),
    }

    // InputResult ok=true and ok=false
    let ok_frame = br#"{"type":"input_result","id":9,"cmd":"tap","ok":true,"elapsed_ms":12,"ts_ms":1700000000000}"#;
    match codec.decode_frame(ok_frame).expect("ok input_result") {
        WireMessage::InputResult(r) => {
            assert_eq!(r.id, 9);
            assert_eq!(r.cmd, "tap");
            assert!(r.ok);
            assert!(r.error.is_none());
            assert_eq!(r.elapsed_ms, 12);
        }
        other => panic!("Expected InputResult, got {other:?}"),
    }

    let err_frame = br#"{"type":"input_result","id":10,"cmd":"swipe","ok":false,"error":"uia_null","elapsed_ms":1,"ts_ms":1700000000001}"#;
    match codec.decode_frame(err_frame).expect("err input_result") {
        WireMessage::InputResult(r) => {
            assert_eq!(r.id, 10);
            assert!(!r.ok);
            assert_eq!(r.error.as_deref(), Some("uia_null"));
        }
        other => panic!("Expected InputResult, got {other:?}"),
    }

    // Encode commands
    let tap = codec
        .encode_command(DaemonCommand::Tap {
            id: 1,
            x: 540,
            y: 1200,
        })
        .expect("encode tap");
    assert_eq!(
        String::from_utf8(tap).unwrap(),
        "{\"cmd\":\"tap\",\"id\":1,\"x\":540,\"y\":1200}\n"
    );

    let swipe = codec
        .encode_command(DaemonCommand::Swipe {
            id: 2,
            x1: 540,
            y1: 1900,
            x2: 540,
            y2: 450,
            duration_ms: 145,
            hold_ms: 25,
        })
        .expect("encode swipe");
    assert_eq!(
        String::from_utf8(swipe).unwrap(),
        "{\"cmd\":\"swipe\",\"id\":2,\"x1\":540,\"y1\":1900,\"x2\":540,\"y2\":450,\"duration_ms\":145,\"hold_ms\":25}\n"
    );

    let key = codec
        .encode_command(DaemonCommand::Keyevent { id: 3, keycode: 4 })
        .expect("encode keyevent");
    assert_eq!(
        String::from_utf8(key).unwrap(),
        "{\"cmd\":\"keyevent\",\"id\":3,\"keycode\":4}\n"
    );

    // Invalid bounds rejected
    assert!(codec
        .encode_command(DaemonCommand::Tap { id: 1, x: -1, y: 10 })
        .is_err());
    assert!(codec
        .encode_command(DaemonCommand::Swipe {
            id: 2,
            x1: 0,
            y1: 0,
            x2: 10,
            y2: 10,
            duration_ms: 5,
            hold_ms: 25
        })
        .is_err());
    assert!(codec
        .encode_command(DaemonCommand::Keyevent { id: 3, keycode: 0 })
        .is_err());
}

async fn spawn_mock_daemon(
    capabilities: Vec<&'static str>,
    fail_swipe: bool,
    drop_on_keyevent: bool,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let received_clone = Arc::clone(&received);

    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let (read_half, mut write_half) = socket.into_split();
        let cap_json = if capabilities.is_empty() {
            String::new()
        } else {
            let quoted: Vec<String> = capabilities.iter().map(|c| format!("\"{c}\"")).collect();
            format!(",\"capabilities\":[{}]", quoted.join(","))
        };
        let hello = format!(
            "{{\"type\":\"hello\",\"protocol_version\":1,\"format\":\"ndjson/1\",\"sdk_int\":37,\"device_model\":\"MockPixel\",\"pid\":100{cap_json}}}\n"
        );
        let _ = write_half.write_all(hello.as_bytes()).await;
        let _ = write_half.flush().await;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim().to_string();
            line.clear();
            if trimmed.is_empty() {
                continue;
            }
            received_clone.lock().unwrap().push(trimmed.clone());
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed) else {
                continue;
            };
            let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
            let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
            if cmd == "keyevent" && drop_on_keyevent {
                drop(write_half);
                return;
            }
            // Interleave a heartbeat frame before the ack to verify framing + demuxing
            let hb = "{\"type\":\"heartbeat\",\"seq\":1,\"ts_ms\":1000,\"uptime_ms\":100,\"package\":\"com.android.settings\",\"window_id\":1,\"clients\":1}\n";
            let _ = write_half.write_all(hb.as_bytes()).await;

            let (ok, err_field) = if cmd == "swipe" && fail_swipe {
                (false, ",\"error\":\"injected_failure\"")
            } else {
                (true, "")
            };
            let ack = format!(
                "{{\"type\":\"input_result\",\"id\":{id},\"cmd\":\"{cmd}\",\"ok\":{ok}{err_field},\"elapsed_ms\":7,\"ts_ms\":1001}}\n"
            );
            let _ = write_half.write_all(ack.as_bytes()).await;
            let _ = write_half.flush().await;
        }
    });

    (port, received)
}

async fn wait_connected_hello(client: &DaemonTcpClient) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let droidpath_core::DaemonConnectionState::Connected { hello: Some(_) } =
            client.state().await
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Client did not receive hello within 2s");
}

#[tokio::test]
async fn tcp_client_routes_native_input_handles_errors_legacy_daemons_and_disconnects() {
    // 1. Happy path + fail_swipe + disconnect on keyevent
    let (port, received) = spawn_mock_daemon(vec!["input"], true, true).await;
    let client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        port,
    )));
    client.start().await;
    wait_connected_hello(&client).await;
    assert!(client.supports_native_input().await);

    let tap_res = client.inject_tap(540, 1200).await.expect("native tap ok");
    assert!(tap_res.ok);
    assert_eq!(tap_res.cmd, "tap");

    let swipe_err = client
        .inject_swipe(540, 1900, 540, 450, 145, 25)
        .await
        .expect_err("mock swipe configured to return ok=false");
    assert_eq!(
        swipe_err,
        DaemonClientError::InputFailed("injected_failure".to_string())
    );

    // Server drops socket on keyevent -> pending waiter fails immediately without waiting 1.5s
    let t0 = Instant::now();
    let key_err = client
        .inject_keyevent(4)
        .await
        .expect_err("drop should fail pending keyevent");
    assert_eq!(key_err, DaemonClientError::NotConnected);
    assert!(
        t0.elapsed() < Duration::from_millis(800),
        "Disconnect should abort waiter promptly, took {:?}",
        t0.elapsed()
    );
    client.stop().await;
    assert_eq!(received.lock().unwrap().len(), 3);

    // 2. Legacy daemon without capabilities -> NotSupported immediately
    let (legacy_port, legacy_rx) = spawn_mock_daemon(vec![], false, false).await;
    let legacy_client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        legacy_port,
    )));
    legacy_client.start().await;
    wait_connected_hello(&legacy_client).await;
    assert!(!legacy_client.supports_native_input().await);
    let err = legacy_client
        .inject_tap(100, 200)
        .await
        .expect_err("legacy daemon must report NotSupported");
    assert_eq!(err, DaemonClientError::NotSupported);
    assert!(
        legacy_rx.lock().unwrap().is_empty(),
        "Client must not send tap to legacy daemon without input capability"
    );
    legacy_client.stop().await;
}

#[tokio::test]
async fn action_executor_uses_daemon_for_matching_serial_and_compresses_recorded_scrolls() {
    let (port, received) = spawn_mock_daemon(vec!["input"], false, false).await;
    let client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        port,
    )));
    client.start().await;
    wait_connected_hello(&client).await;

    // Deliberately provide a non-existent adb path to prove ADB is NEVER called on daemon fast path
    let bogus_adb = DroidPathAdbRunner::new(Some(std::path::PathBuf::from(
        "/nonexistent/adb-should-not-run",
    )));
    let active_serial = Arc::new(RwLock::new(Some("SER_A".to_string())));
    let exec = DroidPathActionExecutor::with_daemon(
        bogus_adb,
        Arc::clone(&client),
        Some(Arc::clone(&active_serial)),
    );

    assert!(exec.supports_zero_fling_swipes("SER_A").await);
    assert!(!exec.supports_zero_fling_swipes("SER_OTHER").await);

    // Tap via daemon
    exec.execute(
        &AttributedUIAction::tap("Settings", Some(180), Some(1150)),
        "SER_A",
    )
    .await
    .expect("tap routed through daemon");

    // Recorded old 500ms explore_scroll_down compressed to <=155ms with hold_ms=25
    exec.execute(
        &AttributedUIAction::swipe("explore_scroll_down", 540, 1900, 540, 450, Some(500)),
        "SER_A",
    )
    .await
    .expect("scroll swipe routed through daemon");

    // Notification dismiss flick uses hold_ms=0
    exec.execute(
        &AttributedUIAction::swipe(
            "Dismiss occluding notification",
            540,
            260,
            540,
            20,
            Some(160),
        ),
        "SER_A",
    )
    .await
    .expect("banner flick routed through daemon");

    // Keyevent BACK -> keycode 4
    exec.execute(&AttributedUIAction::keyevent("BACK"), "SER_A")
        .await
        .expect("keyevent routed through daemon");

    let cmds = received.lock().unwrap().clone();
    assert_eq!(cmds.len(), 4);
    assert!(cmds[0].contains("\"cmd\":\"tap\"") && cmds[0].contains("\"x\":180"));
    assert!(
        cmds[1].contains("\"cmd\":\"swipe\"")
            && cmds[1].contains("\"duration_ms\":155")
            && cmds[1].contains("\"hold_ms\":25"),
        "Unexpected recorded scroll cmd: {}",
        cmds[1]
    );
    assert!(
        cmds[2].contains("\"cmd\":\"swipe\"")
            && cmds[2].contains("\"duration_ms\":160")
            && cmds[2].contains("\"hold_ms\":0"),
        "Unexpected banner flick cmd: {}",
        cmds[2]
    );
    assert!(cmds[3].contains("\"cmd\":\"keyevent\"") && cmds[3].contains("\"keycode\":4"));

    // Mismatched serial bypasses daemon and attempts ADB fallback (which fails cleanly with bogus binary)
    let fallback_err = exec
        .execute(&AttributedUIAction::tap("Settings", Some(180), Some(1150)), "SER_OTHER")
        .await
        .expect_err("mismatched serial should bypass daemon and attempt ADB");
    assert!(matches!(
        fallback_err,
        PathDeviationError::ActionDispatchFailed { .. }
    ));

    client.stop().await;
}

struct ZeroFlingMockExecutor {
    zero_fling: bool,
    recorded: Arc<Mutex<Vec<AttributedUIAction>>>,
    on_action: Arc<dyn Fn(&AttributedUIAction) + Send + Sync>,
}

#[async_trait]
impl MacroActionExecutor for ZeroFlingMockExecutor {
    async fn supports_zero_fling_swipes(&self, _serial: &str) -> bool {
        self.zero_fling
    }

    async fn execute(
        &self,
        action: &AttributedUIAction,
        _serial: &str,
    ) -> Result<(), PathDeviationError> {
        self.recorded.lock().unwrap().push(action.clone());
        (self.on_action)(action);
        Ok(())
    }
}

fn make_node(
    id: i64,
    parent_id: Option<i64>,
    text: &str,
    bounds: &str,
    clickable: bool,
    scrollable: bool,
) -> ShadowUINode {
    let inner = &bounds[1..bounds.len() - 1];
    let (p1, p2) = inner.split_once("][").unwrap();
    let (l, t): (i32, i32) = {
        let (a, b) = p1.split_once(',').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    };
    let (r, b): (i32, i32) = {
        let (a, c) = p2.split_once(',').unwrap();
        (a.parse().unwrap(), c.parse().unwrap())
    };
    ShadowUINode {
        node_id: id,
        parent_id,
        window_id: 1,
        index: id as i32,
        child_index: 0,
        depth: if parent_id.is_some() { 1 } else { 0 },
        package_name: "com.android.settings".to_string(),
        class_name: if scrollable {
            "RecyclerView".to_string()
        } else {
            "TextView".to_string()
        },
        resource_id: format!("com.android.settings:id/n_{id}"),
        text: text.to_string(),
        content_desc: String::new(),
        bounds: bounds.to_string(),
        center_x: (l + r) / 2,
        center_y: (t + b) / 2,
        clickable,
        scrollable,
        editable: false,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

#[tokio::test]
async fn explore_and_macro_use_wide_fast_zero_fling_scroll_and_tight_quiescence() {
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let before_nodes = vec![
        make_node(1, None, "", "[0,400][1080,2400]", false, true),
        make_node(2, Some(1), "Apps", "[40,500][1000,620]", true, false),
    ];
    let after_scroll_nodes = vec![
        make_node(1, None, "", "[0,400][1080,2400]", false, true),
        make_node(3, Some(1), "System", "[40,1400][1000,1520]", true, false),
    ];
    let system_screen_nodes = vec![make_node(
        4,
        None,
        "Developer options",
        "[40,800][1000,920]",
        true,
        false,
    )];

    let feed = |b: Arc<ShadowGraphBridge>, seq: u64, nodes: Vec<ShadowUINode>| async move {
        let snap = WireSnapshotPayload {
            seq,
            timestamp_ms: 1700000000000 + (seq as i64) * 50,
            reason: "test".to_string(),
            active_package: "com.android.settings".to_string(),
            active_window_id: 1,
            node_count: nodes.len(),
            nodes,
            secure_screen_active: Some(false),
        };
        b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
    };

    feed(Arc::clone(&bridge), 1, before_nodes.clone()).await;

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let b_cb = Arc::clone(&bridge);
    let after_s = after_scroll_nodes.clone();
    let sys_s = system_screen_nodes.clone();
    let exec = Arc::new(ZeroFlingMockExecutor {
        zero_fling: true,
        recorded: Arc::clone(&recorded),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b_cb);
            let nodes = match act.kind {
                UIActionKind::Swipe => after_s.clone(),
                UIActionKind::Tap => sys_s.clone(),
                _ => return,
            };
            let seq = if act.kind == UIActionKind::Swipe { 2 } else { 3 };
            tokio::spawn(async move {
                let snap = WireSnapshotPayload {
                    seq,
                    timestamp_ms: 1700000000000 + (seq as i64) * 50,
                    reason: "test".to_string(),
                    active_package: "com.android.settings".to_string(),
                    active_window_id: 1,
                    node_count: nodes.len(),
                    nodes,
                    secure_screen_active: Some(false),
                };
                b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
            });
        }),
    });

    let outcome = bridge
        .explore_and_click(
            "System",
            "SER1",
            exec,
            ExploreOptions {
                max_scrolls: 2,
                step_timeout_ms: 400,
                post_settle_quiescence_ms: 40,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("explore should succeed");
    assert!(matches!(
        outcome,
        ExploreOutcome::ExploredAndClicked {
            scrolls_performed: 1,
            ..
        }
    ));

    let actions = recorded.lock().unwrap().clone();
    let swipe = actions
        .iter()
        .find(|a| a.kind == UIActionKind::Swipe)
        .expect("swipe recorded");
    // Container is [0,400][1080,2400], span_y = 2000 -> 80% = 2000, 18% = 760
    assert_eq!(swipe.y, Some(2000));
    assert_eq!(swipe.end_y, Some(760));
    assert_eq!(swipe.duration_ms, Some(145));

    // Now test MacroExecutionEngine post-swipe quiescence timing (zero_fling=true should be <250ms, not >=480ms!)
    let bridge2 = Arc::new(ShadowGraphBridge::with_default_config());
    feed(Arc::clone(&bridge2), 10, before_nodes.clone()).await;
    let h1 = bridge2
        .hasher
        .fingerprint(&bridge2.dom_state.current_index().await.unwrap())
        .layout_hash;
    feed(Arc::clone(&bridge2), 11, after_scroll_nodes.clone()).await;
    let h2 = bridge2
        .hasher
        .fingerprint(&bridge2.dom_state.current_index().await.unwrap())
        .layout_hash;
    feed(Arc::clone(&bridge2), 12, system_screen_nodes.clone()).await;
    let h3 = bridge2
        .hasher
        .fingerprint(&bridge2.dom_state.current_index().await.unwrap())
        .layout_hash;
    feed(Arc::clone(&bridge2), 13, before_nodes).await;

    let path = vec![
        ScreenTransitionEdge {
            id: uuid::Uuid::new_v4(),
            from_hash: h1,
            to_hash: h2,
            action: Some(AttributedUIAction::swipe(
                "explore_scroll_down",
                540,
                2000,
                540,
                760,
                Some(145),
            )),
            action_signature: "swipe:scroll".to_string(),
            traversal_count: 1,
            average_transition_ms: 150.0,
            is_spanning_tree_edge: true,
            last_traversed_at: chrono::Utc::now(),
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
        },
        ScreenTransitionEdge {
            id: uuid::Uuid::new_v4(),
            from_hash: h2,
            to_hash: h3,
            action: Some(AttributedUIAction::tap("System", Some(520), Some(1460))),
            action_signature: "tap:System".to_string(),
            traversal_count: 1,
            average_transition_ms: 80.0,
            is_spanning_tree_edge: true,
            last_traversed_at: chrono::Utc::now(),
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
        },
    ];

    let b2_cb = Arc::clone(&bridge2);
    let fast_exec = Arc::new(ZeroFlingMockExecutor {
        zero_fling: true,
        recorded: Arc::new(Mutex::new(Vec::new())),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b2_cb);
            let (seq, nodes) = if act.kind == UIActionKind::Swipe {
                (14, after_scroll_nodes.clone())
            } else {
                (15, system_screen_nodes.clone())
            };
            tokio::spawn(async move {
                let snap = WireSnapshotPayload {
                    seq,
                    timestamp_ms: 1700000000000 + (seq as i64) * 50,
                    reason: "test".to_string(),
                    active_package: "com.android.settings".to_string(),
                    active_window_id: 1,
                    node_count: nodes.len(),
                    nodes,
                    secure_screen_active: Some(false),
                };
                b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
            });
        }),
    });

    let engine = MacroExecutionEngine::new(
        Arc::clone(&bridge2.dom_state),
        Arc::clone(&bridge2.graph_engine),
        fast_exec,
        ScreenLayoutHasher::default(),
        MacroExecutionConfiguration {
            step_timeout_ms: 500,
            post_settle_quiescence_ms: 40,
            ..MacroExecutionConfiguration::default()
        },
        None,
    );

    let t0 = Instant::now();
    let report = engine.execute(&path, "SER1").await.expect("macro should arrive");
    let elapsed = t0.elapsed();
    assert_eq!(report.steps.len(), 2);
    assert!(
        elapsed < Duration::from_millis(320),
        "Zero-fling swipe->tap quiescence should be ~85ms (not 480ms ADB wait), took {elapsed:?}"
    );
}

#[tokio::test]
async fn intermediate_scroll_settles_on_one_row_shift_and_explore_drains_mid_swipe_frames() {
    let feed = |b: Arc<ShadowGraphBridge>, seq: u64, nodes: Vec<ShadowUINode>| async move {
        let snap = WireSnapshotPayload {
            seq,
            timestamp_ms: 1700000000000 + (seq as i64) * 50,
            reason: "test".to_string(),
            active_package: "com.android.settings".to_string(),
            active_window_id: 1,
            node_count: nodes.len(),
            nodes,
            secure_screen_active: Some(false),
        };
        b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
    };

    // Part 1: Explore scroll emits an early mid-swipe frame during execute, then a final settled
    // frame 65ms later. Drain-until-quiet must wait for the settled frame and find 'System' there.
    let bridge = Arc::new(ShadowGraphBridge::with_default_config());
    let top_nodes = vec![
        make_node(1, None, "", "[0,400][1080,2400]", false, true),
        make_node(2, Some(1), "Network", "[40,500][1000,620]", true, false),
        make_node(3, Some(1), "Apps", "[40,650][1000,770]", true, false),
    ];
    let mid_swipe_nodes = vec![
        make_node(1, None, "", "[0,400][1080,2400]", false, true),
        make_node(4, Some(1), "Notifications", "[40,500][1000,620]", true, false),
    ];
    let settled_scroll_nodes = vec![
        make_node(1, None, "", "[0,400][1080,2400]", false, true),
        make_node(5, Some(1), "System", "[40,1400][1000,1520]", true, false),
    ];
    let system_nodes = vec![make_node(
        6,
        None,
        "Developer options",
        "[40,600][1000,720]",
        true,
        false,
    )];

    feed(Arc::clone(&bridge), 1, top_nodes.clone()).await;

    let b_cb = Arc::clone(&bridge);
    let mid_s = mid_swipe_nodes.clone();
    let set_s = settled_scroll_nodes.clone();
    let sys_s = system_nodes.clone();
    let exec = Arc::new(ZeroFlingMockExecutor {
        zero_fling: true,
        recorded: Arc::new(Mutex::new(Vec::new())),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b_cb);
            if act.kind == UIActionKind::Swipe {
                let mid = mid_s.clone();
                let fin = set_s.clone();
                tokio::spawn(async move {
                    // Immediate mid-swipe frame, followed 65ms later by the true post-ACTION_UP frame
                    let snap_mid = WireSnapshotPayload {
                        seq: 2,
                        timestamp_ms: 1700000000100,
                        reason: "mid_swipe".to_string(),
                        active_package: "com.android.settings".to_string(),
                        active_window_id: 1,
                        node_count: mid.len(),
                        nodes: mid,
                        secure_screen_active: Some(false),
                    };
                    b.dom_state.handle_message(WireMessage::Snapshot(snap_mid)).await;
                    tokio::time::sleep(Duration::from_millis(65)).await;
                    let snap_fin = WireSnapshotPayload {
                        seq: 3,
                        timestamp_ms: 1700000000165,
                        reason: "settled".to_string(),
                        active_package: "com.android.settings".to_string(),
                        active_window_id: 1,
                        node_count: fin.len(),
                        nodes: fin,
                        secure_screen_active: Some(false),
                    };
                    b.dom_state.handle_message(WireMessage::Snapshot(snap_fin)).await;
                });
            } else if act.kind == UIActionKind::Tap {
                let sys = sys_s.clone();
                tokio::spawn(async move {
                    let snap = WireSnapshotPayload {
                        seq: 4,
                        timestamp_ms: 1700000000300,
                        reason: "tapped".to_string(),
                        active_package: "com.android.settings".to_string(),
                        active_window_id: 1,
                        node_count: sys.len(),
                        nodes: sys,
                        secure_screen_active: Some(false),
                    };
                    b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
                });
            }
        }),
    });

    let outcome = bridge
        .explore_and_click(
            "System",
            "SER1",
            exec,
            ExploreOptions {
                max_scrolls: 2,
                step_timeout_ms: 500,
                post_settle_quiescence_ms: 40,
                prefer_graph_route: false,
                ..ExploreOptions::default()
            },
        )
        .await
        .expect("explore should drain mid-swipe frame and tap System after 1 scroll");
    assert!(matches!(
        outcome,
        ExploreOutcome::ExploredAndClicked {
            scrolls_performed: 1,
            ..
        }
    ));

    // Part 2: Intermediate scroll landing 1 row off (Jaccard 7/9 = 0.7778 in [0.65, 0.80))
    // must still settle cleanly and allow the following Tap step to complete the macro.
    let shared = ["RowB", "RowC", "RowD", "RowE", "RowF", "RowG", "System"];
    let recorded_scroll_labels: Vec<&str> = std::iter::once("RowA").chain(shared.iter().copied()).collect();
    let shifted_scroll_labels: Vec<&str> = shared.iter().copied().chain(std::iter::once("RowH")).collect();

    let mk_list = |base_id: i64, labels: &[&str]| -> Vec<ShadowUINode> {
        let mut out = vec![make_node(1, None, "", "[0,400][1080,2400]", false, true)];
        for (i, lbl) in labels.iter().enumerate() {
            let y0 = 450 + (i as i32) * 140;
            let mut n = make_node(
                base_id + i as i64,
                Some(1),
                lbl,
                &format!("[40,{y0}][1000,{}]", y0 + 110),
                true,
                false,
            );
            n.resource_id = "android:id/title".to_string();
            out.push(n);
        }
        out
    };

    let start_nodes = mk_list(10, &["Header1", "Header2", "Header3"]);
    let rec_scroll_nodes = mk_list(30, &recorded_scroll_labels);
    let shifted_scroll_nodes = mk_list(50, &shifted_scroll_labels);
    let goal_nodes = vec![make_node(99, None, "GoalScreen", "[40,500][1000,620]", true, false)];

    let bridge2 = Arc::new(ShadowGraphBridge::with_default_config());
    feed(Arc::clone(&bridge2), 10, start_nodes.clone()).await;
    let h_start = bridge2.hasher.fingerprint(&bridge2.dom_state.current_index().await.unwrap()).layout_hash;
    // Register recorded scrolled vertex inside SpatialGraphEngine so MacroExecutionEngine can look up its tokens
    let rec_idx = {
        feed(Arc::clone(&bridge2), 11, rec_scroll_nodes.clone()).await;
        bridge2.dom_state.current_index().await.unwrap()
    };
    let h_rec_scroll = bridge2.hasher.fingerprint(&rec_idx).layout_hash;
    let _ = bridge2
        .graph_engine
        .process(&droidpath_core::ShadowDOMUpdate {
            index: rec_idx,
            is_full_snapshot: true,
            inserted_count: rec_scroll_nodes.len(),
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: Some("com.android.settings".to_string()),
            is_secure_screen_active: false,
        })
        .await;
    feed(Arc::clone(&bridge2), 12, goal_nodes.clone()).await;
    let h_goal = bridge2.hasher.fingerprint(&bridge2.dom_state.current_index().await.unwrap()).layout_hash;
    feed(Arc::clone(&bridge2), 13, start_nodes).await;

    let path = vec![
        ScreenTransitionEdge {
            id: uuid::Uuid::new_v4(),
            from_hash: h_start,
            to_hash: h_rec_scroll,
            action: Some(AttributedUIAction::swipe("explore_scroll_down", 540, 2000, 540, 760, Some(145))),
            action_signature: "swipe:scroll".to_string(),
            traversal_count: 1,
            average_transition_ms: 180.0,
            is_spanning_tree_edge: true,
            last_traversed_at: chrono::Utc::now(),
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
        },
        ScreenTransitionEdge {
            id: uuid::Uuid::new_v4(),
            from_hash: h_rec_scroll,
            to_hash: h_goal,
            action: Some(AttributedUIAction::tap("System", Some(520), Some(1350))),
            action_signature: "tap:System".to_string(),
            traversal_count: 1,
            average_transition_ms: 150.0,
            is_spanning_tree_edge: true,
            last_traversed_at: chrono::Utc::now(),
            is_hazardous: false,
            hazard_reason: None,
            consecutive_failures: 0,
        },
    ];

    let b2_cb = Arc::clone(&bridge2);
    let shift_exec = Arc::new(ZeroFlingMockExecutor {
        zero_fling: true,
        recorded: Arc::new(Mutex::new(Vec::new())),
        on_action: Arc::new(move |act| {
            let b = Arc::clone(&b2_cb);
            let (seq, nodes) = if act.kind == UIActionKind::Swipe {
                (14, shifted_scroll_nodes.clone())
            } else {
                (15, goal_nodes.clone())
            };
            tokio::spawn(async move {
                let snap = WireSnapshotPayload {
                    seq,
                    timestamp_ms: 1700000000000 + (seq as i64) * 50,
                    reason: "test".to_string(),
                    active_package: "com.android.settings".to_string(),
                    active_window_id: 1,
                    node_count: nodes.len(),
                    nodes,
                    secure_screen_active: Some(false),
                };
                b.dom_state.handle_message(WireMessage::Snapshot(snap)).await;
            });
        }),
    });

    let engine = MacroExecutionEngine::new(
        Arc::clone(&bridge2.dom_state),
        Arc::clone(&bridge2.graph_engine),
        shift_exec,
        ScreenLayoutHasher::default(),
        MacroExecutionConfiguration {
            step_timeout_ms: 600,
            post_settle_quiescence_ms: 20,
            settle_similarity_threshold: 0.80,
            ..MacroExecutionConfiguration::default()
        },
        None,
    );
    let report = engine
        .execute(&path, "SER1")
        .await
        .expect("intermediate scroll with 1-row shift (Jaccard 0.7778) must settle and tap System");
    assert_eq!(report.steps.len(), 2);
    assert_eq!(report.final_hash, h_goal);
}

#[tokio::test]
async fn tcp_client_drop_cleans_pending_waiter_and_wait_for_hello_enables_cold_start_native_input() {
    // 1. Server sends hello, holds first tap reply indefinitely so dropping the caller future
    //    must clean up pending_inputs via PendingInputGuard, and still answers the second tap.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let (read_half, mut write_half) = socket.into_split();
        let hello = "{\"type\":\"hello\",\"protocol_version\":1,\"format\":\"ndjson/1\",\"sdk_int\":37,\"device_model\":\"MockPixel\",\"pid\":101,\"capabilities\":[\"input\"]}\n";
        let _ = write_half.write_all(hello.as_bytes()).await;
        let _ = write_half.flush().await;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let mut count = 0usize;
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim().to_string();
            line.clear();
            if trimmed.is_empty() {
                continue;
            }
            count += 1;
            if count == 1 {
                // Intentionally never reply to the 1st command so the caller cancels/drops it.
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed) else {
                continue;
            };
            let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
            let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("tap");
            let ack = format!(
                "{{\"type\":\"input_result\",\"id\":{id},\"cmd\":\"{cmd}\",\"ok\":true,\"elapsed_ms\":3,\"ts_ms\":1002}}\n"
            );
            let _ = write_half.write_all(ack.as_bytes()).await;
            let _ = write_half.flush().await;
        }
    });

    let client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        port,
    )));
    client.start().await;
    // Cold-start wait_for_hello must observe hello promptly (<500ms) and unlock supports_native_input.
    let got_hello = client.wait_for_hello(Duration::from_millis(500)).await;
    assert!(got_hello, "wait_for_hello should succeed on live loopback daemon");
    assert!(client.supports_native_input().await);

    // Abort/drop in-flight inject_tap -> PendingInputGuard must remove the waiter immediately.
    let timed = tokio::time::timeout(Duration::from_millis(40), client.inject_tap(100, 200)).await;
    assert!(timed.is_err(), "outer timeout should drop un-replied inject_tap");
    assert_eq!(
        client.pending_input_count(),
        0,
        "Dropping inject_tap future must not leak pending_inputs entry"
    );

    // Next tap still succeeds cleanly.
    let ok = client.inject_tap(300, 400).await.expect("second tap ok");
    assert!(ok.ok);
    assert_eq!(client.pending_input_count(), 0);
    client.stop().await;

    // 2. wait_for_hello on a closed loopback port fails fast on POSIX; on Windows Winsock delays
    //    loopback WSAECONNREFUSED due to TCP SYN retransmission, so bound the timing assertion there.
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);

    let dead_client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        closed_port,
    )));
    dead_client.start().await;
    let t0 = Instant::now();
    let dead_hello = dead_client.wait_for_hello(Duration::from_millis(500)).await;
    assert!(!dead_hello, "wait_for_hello must return false on closed port");
    #[cfg(not(windows))]
    assert!(
        t0.elapsed() < Duration::from_millis(350),
        "wait_for_hello should fail fast on ECONNREFUSED, took {:?}",
        t0.elapsed()
    );
    #[cfg(windows)]
    let _ = t0;
    dead_client.stop().await;

    // 3. Immediate-EOF on an accepted loopback socket fails fast across all platforms (including Windows).
    let eof_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind eof");
    let eof_port = eof_listener.local_addr().unwrap().port();
    let eof_accept = tokio::spawn(async move {
        if let Ok((sock, _)) = eof_listener.accept().await {
            drop(sock);
        }
    });

    let eof_client = Arc::new(DaemonTcpClient::new(DaemonEndpointConfiguration::with_port(
        eof_port,
    )));
    eof_client.start().await;
    let t1 = Instant::now();
    let eof_hello = eof_client.wait_for_hello(Duration::from_millis(500)).await;
    assert!(!eof_hello, "wait_for_hello must return false on immediate EOF");
    assert!(
        t1.elapsed() < Duration::from_millis(350),
        "wait_for_hello should fail fast on immediate EOF, took {:?}",
        t1.elapsed()
    );
    eof_client.stop().await;
    let _ = eof_accept.await;
}
