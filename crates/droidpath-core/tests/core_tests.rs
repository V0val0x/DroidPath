use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use droidpath_core::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
fn make_node(
    node_id: i64,
    parent_id: Option<i64>,
    index: i32,
    package_name: &str,
    class_name: &str,
    resource_id: &str,
    text: &str,
    content_desc: &str,
    clickable: bool,
) -> ShadowUINode {
    ShadowUINode {
        index,
        text: text.to_string(),
        resource_id: resource_id.to_string(),
        content_desc: content_desc.to_string(),
        class_name: class_name.to_string(),
        clickable,
        scrollable: false,
        bounds: "[0,0][100,100]".to_string(),
        center_x: 50,
        center_y: 50,
        node_id,
        parent_id,
        window_id: 1,
        child_index: 0,
        depth: 1,
        package_name: package_name.to_string(),
        editable: false,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

#[test]
fn test_ndjson_wire_codec_decodes_all_messages_and_encodes_commands() {
    let codec = NdjsonWireCodec::new();

    // 1. hello
    let hello_line = br#"{"type":"hello","protocol_version":1,"format":"ndjson/1","sdk_int":36,"device_model":"Pixel 10a","pid":4242}"#;
    match codec.decode_frame(hello_line).unwrap() {
        WireMessage::Hello(h) => {
            assert_eq!(h.protocol_version, 1);
            assert_eq!(h.format, "ndjson/1");
            assert_eq!(h.sdk_int, 36);
            assert_eq!(h.device_model, "Pixel 10a");
            assert_eq!(h.pid, 4242);
        }
        other => panic!("Expected Hello, got {other:?}"),
    }

    // 2. snapshot
    let snap_line = br#"{"type":"snapshot","seq":10,"ts_ms":1700000000,"reason":"initial","package":"com.android.settings","window_id":7,"node_count":1,"nodes":[{"index":0,"text":"Display","resource_id":"com.android.settings:id/title","content_desc":"","class":"TextView","clickable":true,"scrollable":false,"bounds":"[40,200][500,300]","center_x":270,"center_y":250,"node_id":999,"parent_id":null,"window_id":7,"child_index":0,"depth":1,"package_name":"com.android.settings","editable":false,"focused":false,"checked":false,"enabled":true,"visible":true}]}"#;
    match codec.decode_frame(snap_line).unwrap() {
        WireMessage::Snapshot(s) => {
            assert_eq!(s.seq, 10);
            assert_eq!(s.active_package, "com.android.settings");
            assert_eq!(s.nodes.len(), 1);
            assert_eq!(s.nodes[0].node_id, 999);
            assert_eq!(s.nodes[0].parent_id, None);
            assert_eq!(s.nodes[0].to_compact_node().resource_id, "com.android.settings:id/title");
        }
        other => panic!("Expected Snapshot, got {other:?}"),
    }

    // 3. diff with explicit "parent_id": null vs omitted "parent_id"
    let diff_line = br#"{"type":"diff","base_seq":10,"seq":11,"ts_ms":1700000050,"package":"com.android.settings","window_id":7,"event_type":2048,"event_package":"com.android.settings","inserted":[],"updated":[{"node_id":999,"mask":5,"parent_id":null,"text":"Brightness"},{"node_id":1000,"mask":4,"text":"Auto"}],"removed":[555]}"#;
    match codec.decode_frame(diff_line).unwrap() {
        WireMessage::Diff(d) => {
            assert_eq!(d.base_seq, 10);
            assert_eq!(d.seq, 11);
            assert_eq!(d.removed, vec![555]);
            assert_eq!(d.updated.len(), 2);
            assert!(d.updated[0].has_parent_id_key());
            assert_eq!(d.updated[0].parent_id(), None);
            assert_eq!(d.updated[0].text.as_deref(), Some("Brightness"));
            assert!(!d.updated[1].has_parent_id_key());
            assert_eq!(d.updated[1].text.as_deref(), Some("Auto"));
        }
        other => panic!("Expected Diff, got {other:?}"),
    }

    // 4. heartbeat, pong (reordered keys), error, secure_screen
    assert!(matches!(
        codec.decode_frame(br#"{"type":"heartbeat","seq":11,"ts_ms":100,"uptime_ms":500,"package":"com.android.settings","window_id":7,"clients":1}"#).unwrap(),
        WireMessage::Heartbeat(hb) if hb.clients == 1
    ));
    assert!(matches!(
        codec.decode_frame(br#"{"seq":11,"ts_ms":123,"type":"pong"}"#).unwrap(),
        WireMessage::Pong(p) if p.timestamp_ms == 123
    ));
    assert!(matches!(
        codec.decode_frame(br#"{"type":"error","code":"invalid_command","message":"bad","ts_ms":125}"#).unwrap(),
        WireMessage::Error(e) if e.code == "invalid_command"
    ));
    assert!(matches!(
        codec.decode_frame(br#"{"type":"secure_screen","code":"SECURE_SCREEN_ACTIVE","active":true,"reason":"flag_secure_null_root","package":"com.bank.app","ts_ms":1000}"#).unwrap(),
        WireMessage::SecureScreen(s) if s.active && s.code == "SECURE_SCREEN_ACTIVE"
    ));

    // Error cases
    assert!(codec.decode_frame(b"").is_err());
    assert!(codec.decode_frame(br#"{"type":"unknown_msg"}"#).is_err());

    // Command encoding
    assert_eq!(codec.encode_command(DaemonCommand::Snapshot).unwrap(), b"{\"cmd\":\"snapshot\"}\n");
    assert_eq!(codec.encode_command(DaemonCommand::Ping).unwrap(), b"{\"cmd\":\"ping\"}\n");
    assert_eq!(codec.encode_command(DaemonCommand::Shutdown).unwrap(), b"{\"cmd\":\"shutdown\"}\n");
    assert_eq!(
        codec.encode_command(DaemonCommand::SetDebounce { ms: 30 }).unwrap(),
        b"{\"cmd\":\"set_debounce\",\"ms\":30}\n"
    );
    assert!(codec.encode_command(DaemonCommand::SetDebounce { ms: -1 }).is_err());
    assert!(codec.encode_command(DaemonCommand::SetDebounce { ms: 9999 }).is_err());
}

#[test]
fn test_line_framer_handles_utf8_splits_crlf_and_overflow_reset() {
    let mut framer = LineFramer::new(64);
    let line1 = "{\"text\":\"Привет 🚀\"}";
    let line2 = "{\"ok\":true}";
    let combined = format!("{line1}\r\n\n{line2}\n{{\"tail\":1");
    let bytes = combined.as_bytes();

    let mut collected = Vec::new();
    let step_sizes = [3, 5, 2, 7, 4, 1, 8, 50];
    let mut offset = 0usize;
    let mut step_idx = 0usize;
    while offset < bytes.len() {
        let step = step_sizes[step_idx % step_sizes.len()];
        step_idx += 1;
        let end = (offset + step).min(bytes.len());
        collected.extend(framer.append(&bytes[offset..end]).unwrap());
        offset = end;
    }

    assert_eq!(collected.len(), 2);
    assert_eq!(std::str::from_utf8(&collected[0]).unwrap(), line1);
    assert_eq!(std::str::from_utf8(&collected[1]).unwrap(), line2);
    assert_eq!(framer.buffered_byte_count(), "{\"tail\":1".len());

    let tail = framer.append(b"}\n").unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(std::str::from_utf8(&tail[0]).unwrap(), "{\"tail\":1}");
    assert_eq!(framer.buffered_byte_count(), 0);

    let oversized = vec![b'a'; 65];
    assert_eq!(
        framer.append(&oversized),
        Err(LineFramerError::FrameTooLarge {
            byte_count: 65,
            limit: 64
        })
    );
    assert_eq!(framer.buffered_byte_count(), 0);

    let recovered = framer.append(b"{\"recovered\":1}\n").unwrap();
    assert_eq!(recovered.len(), 1);
}

#[tokio::test]
async fn test_shadow_dom_state_snapshot_diff_queries_and_gap_resync() {
    let resync_count = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&resync_count);
    let dom = ShadowDomState::new(
        0.0,
        Some(Arc::new(move || {
            let c = Arc::clone(&counter_clone);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        })),
    );

    // Cold diff triggers resync
    dom.handle_message(WireMessage::Diff(WireDiffPayload {
        base_seq: 1,
        seq: 2,
        timestamp_ms: 100,
        active_package: "com.test".to_string(),
        active_window_id: 1,
        event_type: None,
        event_package: None,
        inserted: vec![],
        updated: vec![],
        removed: vec![],
    }))
    .await;
    assert_eq!(resync_count.load(Ordering::SeqCst), 1);

    let root = make_node(1, None, 0, "com.test", "FrameLayout", "com.test:id/root", "", "", false);
    let label = make_node(2, Some(1), 1, "com.test", "TextView", "com.test:id/label_confirm", "Confirm", "", false);
    let btn = make_node(3, Some(1), 2, "com.test", "Button", "com.test:id/btn_confirm", "Confirm", "", true);
    let banner = make_node(4, Some(1), 3, "com.test", "TextView", "com.test:id/banner", "Old Banner", "", false);

    dom.handle_message(WireMessage::Snapshot(WireSnapshotPayload {
        seq: 10,
        timestamp_ms: 1000,
        reason: "initial".to_string(),
        active_package: "com.test".to_string(),
        active_window_id: 1,
        node_count: 4,
        nodes: vec![root, label, btn, banner],
        secure_screen_active: None,
    }))
    .await;

    assert_eq!(dom.get_current_tree().await.len(), 4);
    assert_eq!(dom.get_compact_tree().await.len(), 4);

    let found_confirm = dom
        .find_node_by_text("Confirm", ShadowTreeMatchMode::ContainsCaseInsensitive, true)
        .await;
    assert_eq!(found_confirm.map(|n| n.node_id), Some(3));

    let found_short = dom
        .find_node_by_resource_id("btn_confirm", ShadowTreeMatchMode::SuffixOrExactResourceId)
        .await;
    assert_eq!(found_short.map(|n| n.node_id), Some(3));

    let child_ids: Vec<i64> = dom.children_of_id(1).await.into_iter().map(|n| n.node_id).collect();
    assert_eq!(child_ids, vec![2, 3, 4]);

    let new_node = make_node(
        5,
        Some(1),
        4,
        "com.test",
        "ImageButton",
        "com.test:id/fab",
        "",
        "Create New Item",
        true,
    );
    let patch = WireNodePatch {
        node_id: 3,
        mask: ShadowPatchMask::PARENT | ShadowPatchMask::TEXT | ShadowPatchMask::BOUNDS,
        parent_id_patch: Some(None),
        text: Some("Submit Order".to_string()),
        bounds: Some("[100,200][300,300]".to_string()),
        center_x: Some(200),
        center_y: Some(250),
        ..Default::default()
    };

    dom.handle_message(WireMessage::Diff(WireDiffPayload {
        base_seq: 10,
        seq: 11,
        timestamp_ms: 1050,
        active_package: "com.test".to_string(),
        active_window_id: 1,
        event_type: None,
        event_package: None,
        inserted: vec![new_node],
        updated: vec![patch],
        removed: vec![4],
    }))
    .await;

    assert!(dom.node_for_id(4).await.is_none());
    let patched = dom.node_for_id(3).await.unwrap();
    assert_eq!(patched.text, "Submit Order");
    assert_eq!(patched.parent_id, None);
    assert_eq!(patched.center_x, 200);

    let desc_match = dom
        .find_node_by_content_desc("create new", ShadowTreeMatchMode::ContainsCaseInsensitive)
        .await;
    assert_eq!(desc_match.map(|n| n.node_id), Some(5));

    // Gap triggers resync without corrupting seq 11
    dom.handle_message(WireMessage::Diff(WireDiffPayload {
        base_seq: 99,
        seq: 100,
        timestamp_ms: 2000,
        active_package: "com.test".to_string(),
        active_window_id: 1,
        event_type: None,
        event_package: None,
        inserted: vec![],
        updated: vec![],
        removed: vec![],
    }))
    .await;
    assert_eq!(resync_count.load(Ordering::SeqCst), 2);
    assert_eq!(dom.get_snapshot_metadata().await.unwrap().seq, 11);
}

#[test]
fn test_screen_layout_hasher_golden_fnv1a_ecommerce_banner_and_overlay_masking() {
    // 1. Exact golden FNV-1a vectors verified against Swift AndroidShadowGraphKit
    let tokens_en: BTreeSet<String> = [
        "Button|com.shop.app:id/buy_btn|Add · <PRICE>",
        "TextView|com.shop.app:id/eta|<ETA>",
        "TextView|com.shop.app:id/title|Artisan Coffee Beans",
        "TextView|com.shop.app:id/weight|<WEIGHT>",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        ScreenLayoutHasher::fnv1a64("com.shop.app", &tokens_en),
        0xb3f4d749d7de68a1u64
    );

    let tokens_ru: BTreeSet<String> = [
        "Button||Оплатить <PRICE>",
        "TextView||Доставка <ETA>",
        "TextView||Привет мир",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        ScreenLayoutHasher::fnv1a64("com.ru.shop", &tokens_ru),
        0xa3b264bb1290e15du64
    );

    // 2. E-commerce prices, ETAs, weights, 5G & Russian year protection
    assert_eq!(ScreenLayoutHasher::normalize_label("15.99€"), "<PRICE>");
    assert_eq!(ScreenLayoutHasher::normalize_label("$1,299.00"), "<PRICE>");
    assert_eq!(ScreenLayoutHasher::normalize_label("€ 9,99"), "<PRICE>");
    assert_eq!(ScreenLayoutHasher::normalize_label("499 ₽"), "<PRICE>");
    assert_eq!(ScreenLayoutHasher::normalize_label("1 250 руб."), "<PRICE>");
    assert_eq!(
        ScreenLayoutHasher::normalize_label("Order 2 items 250 р."),
        "Order # items <PRICE>"
    );
    assert_eq!(ScreenLayoutHasher::normalize_label("15-25 min"), "<ETA>");
    assert_eq!(ScreenLayoutHasher::normalize_label("~20 мин"), "<ETA>");
    assert_eq!(ScreenLayoutHasher::normalize_label("1 hr 20 min"), "<ETA>");
    assert_eq!(ScreenLayoutHasher::normalize_label("ETA 14:30"), "<ETA>");
    assert_eq!(ScreenLayoutHasher::normalize_label("250g"), "<WEIGHT>");
    assert_eq!(ScreenLayoutHasher::normalize_label("1.5 kg"), "<WEIGHT>");
    assert_eq!(ScreenLayoutHasher::normalize_label("500 г"), "<WEIGHT>");
    assert_eq!(ScreenLayoutHasher::normalize_label("330 ml"), "<WEIGHT>");
    assert_eq!(ScreenLayoutHasher::normalize_label("5G"), "#G");
    assert_eq!(ScreenLayoutHasher::normalize_label("15 нояб. 2026 г."), "<DATE>");
    assert_eq!(
        ScreenLayoutHasher::normalize_label("Sun, May 15"),
        "<DAY>, <DATE>"
    );

    // 3. Full product screen invariance
    let hasher = ScreenLayoutHasher::default();
    let idx_a = ShadowTreeIndex::new(
        1,
        100,
        "com.shop.app",
        1,
        vec![
            make_node(1, None, 0, "com.shop.app", "TextView", "id/title", "Artisan Coffee Beans", "", false),
            make_node(2, None, 1, "com.shop.app", "TextView", "id/weight", "250g", "", false),
            make_node(3, None, 2, "com.shop.app", "TextView", "id/eta", "15-25 min", "", false),
            make_node(4, None, 3, "com.shop.app", "Button", "id/buy_btn", "Add · 15.99€", "", true),
        ],
    );
    let idx_b = ShadowTreeIndex::new(
        2,
        200,
        "com.shop.app",
        1,
        vec![
            make_node(1, None, 0, "com.shop.app", "TextView", "id/title", "Artisan Coffee Beans", "", false),
            make_node(2, None, 1, "com.shop.app", "TextView", "id/weight", "1.2 kg", "", false),
            make_node(3, None, 2, "com.shop.app", "TextView", "id/eta", "~40 мин", "", false),
            make_node(4, None, 3, "com.shop.app", "Button", "id/buy_btn", "Add · 1 490 руб.", "", true),
        ],
    );
    assert_eq!(
        hasher.fingerprint(&idx_a).layout_hash,
        hasher.fingerprint(&idx_b).layout_hash
    );

    // 4. Visual noise reduction (#6) & Overlay masking (#3)
    let base_nodes = vec![
        make_node(1, None, 0, "com.shop.app", "TextView", "id/header", "Catalog", "", false),
        make_node(2, None, 1, "com.shop.app", "Button", "id/cart", "Cart", "", true),
        make_node(3, None, 2, "com.shop.app", "ImageView", "id/logo", "", "Brand", false),
        make_node(4, None, 3, "com.shop.app", "AppCompatImageView", "", "", "Search", true),
        make_node(5, None, 4, "com.shop.app", "ImageView", "", "", "Winter Sale 50%", false),
    ];
    let mut overlay_nodes = base_nodes[..4].to_vec();
    overlay_nodes.push(make_node(
        99,
        None,
        4,
        "com.android.systemui",
        "FrameLayout",
        "com.android.systemui:id/expandableNotificationRow",
        "Incoming Telegram message",
        "",
        true,
    ));

    let fp_base = hasher.fingerprint(&ShadowTreeIndex::new(1, 10, "com.shop.app", 1, base_nodes));
    let fp_overlay = hasher.fingerprint(&ShadowTreeIndex::new(
        3,
        30,
        "com.android.systemui",
        99,
        overlay_nodes,
    ));
    assert_eq!(fp_overlay.active_package, "com.shop.app");
    assert_eq!(fp_overlay.layout_hash, fp_base.layout_hash);
    assert!(fp_base.tokens.contains("ImageView|id/logo|Brand"));
    assert!(fp_base.tokens.contains("AppCompatImageView||Search"));
}

#[tokio::test]
async fn test_spatial_graph_compaction_exact_threshold_and_spanning_dag_rebuild() {
    let now = Utc::now();
    let root_hash = 100u64;
    let shortcut_hub = 101u64;
    let settings = 102u64;
    let display = 103u64;
    let timeout_screen = 104u64;
    let implicit_leaf = 105u64;
    let post_implicit = 106u64;

    let make_tokens = |prefix: &str| -> BTreeSet<String> {
        (0..10).map(|i| format!("TextView||{prefix}_{i}")).collect()
    };

    let vertices = vec![
        ScreenStateVertex {
            layout_hash: root_hash,
            active_package: "com.launcher".into(),
            sample_interactive_labels: vec!["Home".into()],
            semantic_tokens: make_tokens("Home"),
            first_seen_at: now,
            last_seen_at: now,
            visit_count: 5,
            discovery_parent_hash: None,
            discovery_depth: 0,
        },
        ScreenStateVertex {
            layout_hash: shortcut_hub,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["Hub".into()],
            semantic_tokens: make_tokens("Hub"),
            first_seen_at: now + ChronoDuration::seconds(1),
            last_seen_at: now,
            visit_count: 1,
            discovery_parent_hash: Some(root_hash),
            discovery_depth: 1,
        },
        ScreenStateVertex {
            layout_hash: settings,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["Settings".into()],
            semantic_tokens: make_tokens("Settings"),
            first_seen_at: now + ChronoDuration::seconds(2),
            last_seen_at: now,
            visit_count: 4,
            discovery_parent_hash: Some(root_hash),
            discovery_depth: 1,
        },
        ScreenStateVertex {
            layout_hash: display,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["Display".into()],
            semantic_tokens: make_tokens("Display"),
            first_seen_at: now + ChronoDuration::seconds(3),
            last_seen_at: now,
            visit_count: 3,
            discovery_parent_hash: Some(settings),
            discovery_depth: 2,
        },
        ScreenStateVertex {
            layout_hash: timeout_screen,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["Screen timeout".into()],
            semantic_tokens: make_tokens("Timeout"),
            first_seen_at: now + ChronoDuration::seconds(4),
            last_seen_at: now,
            visit_count: 2,
            discovery_parent_hash: Some(shortcut_hub),
            discovery_depth: 2,
        },
        ScreenStateVertex {
            layout_hash: implicit_leaf,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["ImplicitLeaf".into()],
            semantic_tokens: make_tokens("ImplicitLeaf"),
            first_seen_at: now + ChronoDuration::seconds(5),
            last_seen_at: now,
            visit_count: 1,
            discovery_parent_hash: Some(root_hash),
            discovery_depth: 1,
        },
        ScreenStateVertex {
            layout_hash: post_implicit,
            active_package: "com.settings".into(),
            sample_interactive_labels: vec!["PostImplicit".into()],
            semantic_tokens: make_tokens("PostImplicit"),
            first_seen_at: now + ChronoDuration::seconds(6),
            last_seen_at: now,
            visit_count: 1,
            discovery_parent_hash: Some(implicit_leaf),
            discovery_depth: 2,
        },
    ];

    let mk_edge = |from: u64, to: u64, label: Option<&str>, spanning: bool| ScreenTransitionEdge {
        id: Uuid::new_v4(),
        from_hash: from,
        to_hash: to,
        action: label.map(|l| AttributedUIAction::tap(l, None, None)),
        action_signature: label
            .map(|l| format!("tap:{l}"))
            .unwrap_or_else(|| "implicit".into()),
        traversal_count: 1,
        average_transition_ms: 50.0,
        is_spanning_tree_edge: spanning,
        last_traversed_at: now,
        is_hazardous: false,
        hazard_reason: None,
        consecutive_failures: 0,
    };

    let edges = vec![
        mk_edge(root_hash, shortcut_hub, Some("Hub"), true),
        mk_edge(shortcut_hub, timeout_screen, None, true),
        mk_edge(root_hash, settings, Some("Settings"), true),
        mk_edge(settings, display, Some("Display"), true),
        mk_edge(display, timeout_screen, Some("Screen timeout"), false),
        mk_edge(root_hash, implicit_leaf, None, false),
        mk_edge(implicit_leaf, post_implicit, Some("PostImplicit"), false),
    ];

    let mut spanning_children = BTreeMap::new();
    spanning_children.insert(root_hash, vec![shortcut_hub, settings]);
    spanning_children.insert(shortcut_hub, vec![timeout_screen]);
    spanning_children.insert(settings, vec![display]);

    let snap = SpatialGraphSnapshot {
        root_state_hash: Some(root_hash),
        current_state_hash: Some(root_hash),
        vertices,
        edges,
        spanning_children,
    };

    let engine = SpatialGraphEngine::default();
    engine.import_snapshot(snap).await;

    assert!(engine.is_spanning_dag_acyclic().await);
    let rebuilt = engine.snapshot().await;
    let pf = AStarPathfinder::default();
    let spanning_route = pf
        .find_spanning_dag_path(root_hash, timeout_screen, &rebuilt)
        .expect("executable spanning path must exist");
    let to_hashes: Vec<u64> = spanning_route.edges.iter().map(|e| e.to_hash).collect();
    assert_eq!(to_hashes, vec![settings, display, timeout_screen]);
    assert_eq!(
        engine.spanning_path_from_root(post_implicit).await,
        vec![root_hash, implicit_leaf, post_implicit]
    );
}

#[tokio::test]
async fn test_daemon_tcp_client_and_shadow_graph_bridge_live_loopback_streaming() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();

        let hello = "{\"type\":\"hello\",\"protocol_version\":1,\"format\":\"ndjson/1\",\"sdk_int\":36,\"device_model\":\"Pixel Test\",\"pid\":101}\n";
        let snap = "{\"type\":\"snapshot\",\"seq\":1,\"ts_ms\":1000,\"reason\":\"initial\",\"package\":\"com.android.settings\",\"window_id\":1,\"node_count\":1,\"nodes\":[{\"index\":0,\"text\":\"Display\",\"resource_id\":\"com.android.settings:id/display\",\"content_desc\":\"\",\"class\":\"TextView\",\"clickable\":true,\"scrollable\":false,\"bounds\":\"[10,10][200,80]\",\"center_x\":105,\"center_y\":45,\"node_id\":1,\"parent_id\":null,\"window_id\":1,\"child_index\":0,\"depth\":1,\"package_name\":\"com.android.settings\",\"editable\":false,\"focused\":false,\"checked\":false,\"enabled\":true,\"visible\":true}]}\n";

        // Send fragmented bytes across writes
        writer.write_all(&hello.as_bytes()[..20]).await.unwrap();
        writer.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        writer.write_all(&hello.as_bytes()[20..]).await.unwrap();
        writer.write_all(snap.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf_reader = BufReader::new(reader);
        let mut cmd_line = String::new();
        if let Ok(n) = buf_reader.read_line(&mut cmd_line).await {
            if n > 0 && cmd_line.contains("\"ping\"") {
                writer
                    .write_all(b"{\"type\":\"pong\",\"ts_ms\":1050,\"seq\":1}\n")
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let bridge = ShadowGraphBridge::new(
        DaemonEndpointConfiguration::with_port(port),
        ScreenLayoutHasher::default(),
        0.72,
    );
    bridge.start().await;

    // Immediately after start() (before background task has polled), state must already be Connecting/Connected
    // so fast_compact_tree_if_fresh waits for the initial snapshot instead of returning None in 0ms.
    assert!(matches!(
        bridge.client.state().await,
        DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. }
    ));
    let compact = bridge
        .fast_compact_tree_if_fresh(2.0, 15.0)
        .await
        .expect("fast_compact_tree_if_fresh must await cold-start snapshot");
    assert_eq!(compact[0].text, "Display");

    let snap = bridge
        .dom_state
        .wait_for_snapshot(2.0)
        .await
        .expect("expected live snapshot over loopback TCP");
    assert_eq!(snap.active_package, "com.android.settings");
    assert_eq!(snap.ordered_nodes.len(), 1);
    assert!(bridge.ensure_shadow_stream_ready(0.2, 15.0).await);

    bridge.client.ping().await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    assert_eq!(bridge.graph_engine.vertex_count().await, 1);

    bridge.stop().await;
    server_task.abort();
}
