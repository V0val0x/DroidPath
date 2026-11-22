use std::sync::Arc;

use droidpath_core::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

#[test]
fn test_adb_validation_quoting_device_parsing_and_keycodes() {
    assert!(DroidPathAdbRunner::validate_serial("61061JEA307110"));
    assert!(DroidPathAdbRunner::validate_serial("192.168.1.42:5555"));
    assert!(DroidPathAdbRunner::validate_serial("adb-61061JEA307110-abc._tls-connect"));
    assert!(!DroidPathAdbRunner::validate_serial(""));
    assert!(!DroidPathAdbRunner::validate_serial("-s"));
    assert!(!DroidPathAdbRunner::validate_serial("6106; rm -rf /"));
    assert!(!DroidPathAdbRunner::validate_serial("serial with spaces"));

    let sample = "\
* daemon started successfully
List of devices attached
61061JEA307110         device usb:1-1 product:tegu model:Pixel_10a device:tegu transport_id:4
192.168.1.80:5555      device product:panther model:Pixel_7 device:panther transport_id:5
offline_serial         offline transport_id:6
";
    let parsed = DroidPathAdbRunner::parse_devices_output(sample);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].serial, "61061JEA307110");
    assert_eq!(parsed[0].model, "Pixel 10a");
    assert!(!parsed[0].is_wireless);
    assert_eq!(parsed[1].serial, "192.168.1.80:5555");
    assert!(parsed[1].is_wireless);

    assert_eq!(
        DroidPathAdbRunner::build_safe_type_text_command("hello; rm -rf /"),
        (false, "'hello;%srm%s-rf%s/'".to_string())
    );
    assert_eq!(
        DroidPathAdbRunner::build_safe_type_text_command("it's $(id)\0"),
        (false, "'it'\\''s%s$(id)'".to_string())
    );
    assert_eq!(
        DroidPathAdbRunner::build_safe_type_text_command("Привет мир"),
        (true, "'Привет мир'".to_string())
    );

    assert!(DroidPathAdbRunner::validate_package_name("com.android.settings"));
    assert!(!DroidPathAdbRunner::validate_package_name("com.foo;id"));
    assert!(!DroidPathAdbRunner::validate_package_name("-flag"));

    assert_eq!(DroidPathAdbRunner::resolve_key_code("HOME").as_deref(), Some("3"));
    assert_eq!(DroidPathAdbRunner::resolve_key_code("KEYCODE_APP_SWITCH").as_deref(), Some("187"));
    assert_eq!(DroidPathAdbRunner::resolve_key_code("power").as_deref(), Some("26"));
    assert_eq!(DroidPathAdbRunner::resolve_key_code("66").as_deref(), Some("66"));
    assert!(DroidPathAdbRunner::resolve_key_code("4;id").is_none());
}

#[test]
fn test_swift_andonmac_shadow_graph_json_100_percent_bidirectional_compatibility() {
    // Exact golden JSON emitted by Swift Foundation JSONEncoder (.iso8601 + alternating spanningChildren array)
    let swift_golden = r#"{
  "currentStateHash" : 200,
  "edges" : [
    {
      "action" : {
        "kind" : "tap",
        "selectorResourceId" : "icon",
        "selectorText" : "Settings",
        "targetDescription" : "Settings",
        "timestamp" : "2023-11-14T22:13:20Z",
        "x" : 150,
        "y" : 250
      },
      "actionSignature" : "tap:Settings",
      "averageTransitionMs" : 120.5,
      "fromHash" : 100,
      "id" : "11111111-2222-3333-4444-555555555555",
      "isHazardous" : false,
      "isSpanningTreeEdge" : true,
      "lastTraversedAt" : "2023-11-14T22:13:30Z",
      "toHash" : 200,
      "traversalCount" : 1
    }
  ],
  "rootStateHash" : 100,
  "spanningChildren" : [
    100,
    [
      200
    ]
  ],
  "vertices" : [
    {
      "activePackage" : "com.launcher",
      "discoveryDepth" : 0,
      "firstSeenAt" : "2023-11-14T22:13:20Z",
      "lastSeenAt" : "2023-11-14T22:13:20Z",
      "layoutHash" : 100,
      "sampleInteractiveLabels" : [
        "Settings"
      ],
      "semanticTokens" : [
        "TextView|icon|Settings"
      ],
      "visitCount" : 2
    },
    {
      "activePackage" : "com.settings",
      "discoveryDepth" : 1,
      "discoveryParentHash" : 100,
      "firstSeenAt" : "2023-11-14T22:13:30Z",
      "lastSeenAt" : "2023-11-14T22:13:30Z",
      "layoutHash" : 200,
      "sampleInteractiveLabels" : [
        "Display"
      ],
      "semanticTokens" : [
        "TextView|disp|Display"
      ],
      "visitCount" : 1
    }
  ]
}"#;

    // 1. Rust decodes Swift's golden JSON cleanly
    let snap: SpatialGraphSnapshot = serde_json::from_str(swift_golden).unwrap();
    assert_eq!(snap.root_state_hash, Some(100));
    assert_eq!(snap.current_state_hash, Some(200));
    assert_eq!(snap.vertices.len(), 2);
    assert_eq!(snap.edges.len(), 1);
    assert_eq!(snap.spanning_children.get(&100), Some(&vec![200u64]));
    assert_eq!(
        snap.edges[0].id,
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap()
    );

    // 2. Rust re-serializes into Swift-compatible JSON (alternating array, uppercase UUID, ISO-8601 seconds no fractional)
    let reencoded = serde_json::to_string(&snap).unwrap();
    let val: serde_json::Value = serde_json::from_str(&reencoded).unwrap();
    assert_eq!(val["spanningChildren"], serde_json::json!([100, [200]]));
    assert_eq!(
        val["edges"][0]["id"],
        serde_json::json!("11111111-2222-3333-4444-555555555555")
    );
    assert_eq!(
        val["vertices"][0]["firstSeenAt"],
        serde_json::json!("2023-11-14T22:13:20Z")
    );
    assert!(val["vertices"][0].get("discoveryParentHash").is_none());

    // 3. Also accepts JSON object map for spanningChildren & legacy omitted fields
    let map_variant = r#"{
        "rootStateHash": 100,
        "vertices": [{
            "layoutHash": 100,
            "activePackage": "com.a",
            "sampleInteractiveLabels": [],
            "firstSeenAt": "2023-11-14T22:13:20Z",
            "lastSeenAt": "2023-11-14T22:13:20Z",
            "visitCount": 1,
            "discoveryDepth": 0
        }],
        "edges": [],
        "spanningChildren": {"100": [200]}
    }"#;
    let map_snap: SpatialGraphSnapshot = serde_json::from_str(map_variant).unwrap();
    assert_eq!(map_snap.spanning_children.get(&100), Some(&vec![200u64]));
    assert!(map_snap.vertices[0].semantic_tokens.is_empty());
}

#[test]
fn test_xml_fallback_parser_and_scrcpy_control_encoders() {
    let xml = r#"<hierarchy rotation="0">
        <node text="Settings" resource-id="com.launcher:id/settings" content-desc="" class="android.widget.TextView" clickable="true" scrollable="false" bounds="[100,200][300,400]" />
        <node text="" resource-id="" content-desc="" class="android.widget.FrameLayout" clickable="false" scrollable="false" bounds="[0,0][1080,2400]" />
    </hierarchy>"#;
    let nodes = DroidPathUITreeFallbackParser::parse(xml);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].text, "Settings");
    assert_eq!(nodes[0].class_name, "TextView");
    assert_eq!(nodes[0].center_x, 200);
    assert_eq!(nodes[0].center_y, 300);

    let touch = ScrcpyBridge::encode_touch_event(0, 0, 540, 1200, 1080, 2400, 1.0);
    assert_eq!(touch.len(), 32);
    assert_eq!(touch[0], 2);
    assert_eq!(touch[1], 0);

    let scroll = ScrcpyBridge::encode_scroll_event(540, 1200, 1080, 2400, 0.0, -0.5);
    assert_eq!(scroll.len(), 21);
    assert_eq!(scroll[0], 3);

    let key = ScrcpyBridge::encode_keycode(3, 0, 0);
    assert_eq!(key.len(), 14);
    assert_eq!(key[0], 0);

    // H.264 Baseline Profile SPS for 1920x1080 (with frame cropping 1088 -> 1080)
    let sps_1920x1080 = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x28, 0xd9, 0x00, 0x78, 0x02, 0x27, 0xe5, 0x84,
        0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x60, 0xc9, 0x20,
    ];
    assert_eq!(
        parse_h264_sps_dimensions(&sps_1920x1080),
        Some((1920, 1080))
    );

    // H.264 Baseline Profile SPS for 1280x720 (no cropping: 80x45 macroblocks)
    let sps_1280x720 = [
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x80, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xe8, 0x06, 0xd0,
        0xa1, 0x35,
    ];
    assert_eq!(parse_h264_sps_dimensions(&sps_1280x720), Some((1280, 720)));

    // Non-SPS NAL unit returns None
    assert_eq!(
        parse_h264_sps_dimensions(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]),
        None
    );

    // Minimal Stored ZIP containing classes.dex with scrcpy version & send_stream_meta
    let dex_payload =
        b"dex\n035\0...send_stream_meta...The server version (5.0.1) does not match the client (";
    let zip = build_stored_zip("classes.dex", dex_payload);
    let inspected = ScrcpyBridge::inspect_scrcpy_server_jar(&zip);
    assert_eq!(inspected.version.as_deref(), Some("5.0.1"));
    assert_eq!(inspected.meta_option, ScrcpyMetaOption::SendStreamMeta);
}

fn build_stored_zip(entry_name: &str, data: &[u8]) -> Vec<u8> {
    let name_bytes = entry_name.as_bytes();
    let mut out = Vec::new();
    // Local file header
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method = 0 (stored)
    out.extend_from_slice(&0u16.to_le_bytes()); // mod time
    out.extend_from_slice(&0u16.to_le_bytes()); // mod date
    out.extend_from_slice(&0u32.to_le_bytes()); // crc32
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncomp size
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra len
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(data);

    let cd_offset = out.len() as u32;
    // Central directory header
    out.extend_from_slice(b"PK\x01\x02");
    out.extend_from_slice(&20u16.to_le_bytes()); // version made by
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method = 0
    out.extend_from_slice(&0u16.to_le_bytes()); // time
    out.extend_from_slice(&0u16.to_le_bytes()); // date
    out.extend_from_slice(&0u32.to_le_bytes()); // crc32
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra
    out.extend_from_slice(&0u16.to_le_bytes()); // comment
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // int attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // ext attrs
    out.extend_from_slice(&0u32.to_le_bytes()); // local header offset = 0
    out.extend_from_slice(name_bytes);
    let cd_size = (out.len() as u32) - cd_offset;

    // EOCD
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[tokio::test]
async fn test_mcp_server_initialize_tools_list_stats_and_persistence_roundtrip() {
    let tmp_dir = std::env::temp_dir().join(format!("droidpath-rs-test-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
    let graph_path = tmp_dir.join("shadow_graph.json");

    let manager1 = DroidPathManager::new(
        0,
        graph_path.clone(),
        None,
        DroidPathAdbRunner::default(),
        None,
        None,
    );
    let server = DroidPathMcpServerCore::new(Arc::clone(&manager1));

    let init_resp = server
        .handle_raw_request_data(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await
        .unwrap();
    let init_str = String::from_utf8(init_resp).unwrap();
    assert!(init_str.contains("droidpath-mcp"));
    assert!(init_str.contains("2025-03-26"));

    let legacy_init_resp = server
        .handle_raw_request_data(
            br#"{"jsonrpc":"2.0","id":11,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        )
        .await
        .unwrap();
    let legacy_init_str = String::from_utf8(legacy_init_resp).unwrap();
    assert!(legacy_init_str.contains("\"protocolVersion\":\"2024-11-05\""));

    let list_resp = server
        .handle_raw_request_data(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .await
        .unwrap();
    let list_str = String::from_utf8(list_resp).unwrap();
    assert!(list_str.contains("android_navigate_to_node"));
    assert!(list_str.contains("android_graph_stats"));
    assert!(list_str.contains("confirmed"));

    let list_val: serde_json::Value = serde_json::from_str(&list_str).unwrap();
    let tools = list_val["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 9);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let ann = tool["annotations"].as_object().expect("missing annotations");
        assert!(
            ann.get("title").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
            "tool {name} missing annotations.title"
        );
        for hint in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(ann.get(hint).and_then(|v| v.as_bool()).is_some(), "tool {name} missing {hint}");
        }
        let props = tool["inputSchema"]["properties"].as_object().unwrap();
        for (prop_name, prop_schema) in props {
            assert!(
                prop_schema
                    .get("description")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty()),
                "tool {name} property {prop_name} missing description"
            );
        }
    }

    // Populate graph & persist
    let screen_a = Arc::new(ShadowTreeIndex::new(
        1,
        1000,
        "com.android.launcher",
        1,
        vec![ShadowUINode {
            index: 0,
            text: "Settings".into(),
            resource_id: "icon_settings".into(),
            content_desc: String::new(),
            class_name: "TextView".into(),
            clickable: true,
            scrollable: false,
            bounds: "[100,200][200,300]".into(),
            center_x: 150,
            center_y: 250,
            node_id: 1,
            parent_id: None,
            window_id: 1,
            child_index: 0,
            depth: 1,
            package_name: "com.android.launcher".into(),
            editable: false,
            focused: false,
            checked: false,
            enabled: true,
            visible: true,
        }],
    ));
    let screen_b = Arc::new(ShadowTreeIndex::new(
        2,
        2000,
        "com.android.settings",
        2,
        vec![ShadowUINode {
            index: 0,
            text: "Night Light".into(),
            resource_id: "night_light".into(),
            content_desc: String::new(),
            class_name: "TextView".into(),
            clickable: true,
            scrollable: false,
            bounds: "[50,400][600,500]".into(),
            center_x: 325,
            center_y: 450,
            node_id: 2,
            parent_id: None,
            window_id: 2,
            child_index: 0,
            depth: 1,
            package_name: "com.android.settings".into(),
            editable: false,
            focused: false,
            checked: false,
            enabled: true,
            visible: true,
        }],
    ));

    manager1
        .bridge
        .graph_engine
        .process(&ShadowDOMUpdate {
            index: screen_a,
            is_full_snapshot: true,
            inserted_count: 1,
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: None,
            is_secure_screen_active: false,
        })
        .await;
    manager1
        .record_action(AttributedUIAction::tap("Settings", Some(150), Some(250)))
        .await;
    manager1
        .bridge
        .graph_engine
        .process(&ShadowDOMUpdate {
            index: screen_b,
            is_full_snapshot: true,
            inserted_count: 1,
            updated_count: 0,
            removed_count: 0,
            event_type: None,
            event_package: None,
            is_secure_screen_active: false,
        })
        .await;
    manager1.flush_persisted_graph().await;
    assert!(graph_path.is_file());

    // Reload from disk in fresh manager
    let manager2 = DroidPathManager::new(
        0,
        graph_path.clone(),
        None,
        DroidPathAdbRunner::default(),
        None,
        None,
    );
    let stats = manager2.graph_stats().await;
    assert_eq!(stats.vertex_count, 2);
    assert_eq!(stats.edge_count, 1);

    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
}

async fn raw_http_exchange(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut resp = Vec::new();
    let _ = stream.read_to_end(&mut resp).await;
    String::from_utf8_lossy(&resp).into_owned()
}

#[tokio::test]
async fn test_live_http_sse_server_auth_ipv6_host_negative_cl_and_sse_roundtrip() {
    assert!(DroidPathHttpServer::is_allowed_loopback_host(Some("127.0.0.1:8766")));
    assert!(DroidPathHttpServer::is_allowed_loopback_host(Some("localhost")));
    assert!(DroidPathHttpServer::is_allowed_loopback_host(Some("[::1]:8766")));
    assert!(DroidPathHttpServer::is_allowed_loopback_host(Some("[::1]")));
    assert!(!DroidPathHttpServer::is_allowed_loopback_host(Some("evil.com")));
    assert!(!DroidPathHttpServer::is_allowed_loopback_host(None));

    assert!(DroidPathHttpServer::is_allowed_origin(None));
    assert!(DroidPathHttpServer::is_allowed_origin(Some("http://127.0.0.1:3000")));
    assert!(DroidPathHttpServer::is_allowed_origin(Some("http://[::1]:3000")));
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("https://evil.example.com")));
    assert!(!DroidPathHttpServer::is_allowed_origin(Some("javascript:alert(1)")));

    let tmp_dir = std::env::temp_dir().join(format!("droidpath-http-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
    let mgr = DroidPathManager::new(
        0,
        tmp_dir.join("graph.json"),
        None,
        DroidPathAdbRunner::default(),
        None,
        None,
    );
    let core = Arc::new(DroidPathMcpServerCore::new(mgr));
    let http = DroidPathHttpServer::new(core);
    let port = http.start(0, Some("secret-token".to_string())).await.unwrap();
    assert!(port > 0);

    // 1. Unauthorized -> 401
    let unauth = raw_http_exchange(
        port,
        &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    )
    .await;
    assert!(unauth.contains("401 Unauthorized"), "Got: {unauth}");

    // 2. Negative Content-Length -> 400 Bad Request
    let bad_cl = raw_http_exchange(
        port,
        &format!(
            "POST /message HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer secret-token\r\nContent-Length: -1\r\n\r\n"
        ),
    )
    .await;
    assert!(bad_cl.contains("400 Bad Request"), "Got: {bad_cl}");

    // 3. IPv6 loopback Host [::1]:port -> 200 OK
    let ping_body = r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
    let ipv6_resp = raw_http_exchange(
        port,
        &format!(
            "POST /message HTTP/1.1\r\nHost: [::1]:{port}\r\nAuthorization: Bearer secret-token\r\nContent-Length: {}\r\n\r\n{ping_body}",
            ping_body.len()
        ),
    )
    .await;
    assert!(ipv6_resp.contains("200 OK"), "Got: {ipv6_resp}");
    assert!(ipv6_resp.contains(r#""id":7"#), "Got: {ipv6_resp}");

    // 4. Full SSE handshake + POST /message?sessionId=... -> 202 Accepted + SSE message
    let mut sse_stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sse_stream
        .write_all(
            format!(
                "GET /sse HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer secret-token\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = [0u8; 4096];
    let n = sse_stream.read(&mut buf).await.unwrap();
    let handshake = String::from_utf8_lossy(&buf[..n]);
    assert!(handshake.contains("event: endpoint"), "Got: {handshake}");
    let session_id = handshake
        .split("sessionId=")
        .nth(1)
        .unwrap()
        .trim()
        .to_string();

    let init_body = r#"{"jsonrpc":"2.0","id":99,"method":"initialize","params":{}}"#;
    let post_resp = raw_http_exchange(
        port,
        &format!(
            "POST /message?sessionId={session_id} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer secret-token\r\nContent-Length: {}\r\n\r\n{init_body}",
            init_body.len()
        ),
    )
    .await;
    assert!(post_resp.contains("202 Accepted"), "Got: {post_resp}");

    let n2 = sse_stream.read(&mut buf).await.unwrap();
    let sse_msg = String::from_utf8_lossy(&buf[..n2]);
    assert!(sse_msg.contains("event: message"), "Got: {sse_msg}");
    assert!(sse_msg.contains("droidpath-mcp"), "Got: {sse_msg}");

    http.stop().await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
}
