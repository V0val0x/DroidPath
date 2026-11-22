use std::sync::Arc;
use std::time::Duration;

use droidpath_core::{
    AgenticFeedbackBuilder, DaemonCommand, DaemonEndpointConfiguration, DaemonTcpClient,
    DroidPathAdbRunner, DroidPathHttpServer, DroidPathManager, DroidPathMcpServerCore,
    NdjsonWireCodec, ShadowTreeIndex, ShadowUINode, ToolSchemaExporter, ToolSchemaFormat,
    WireCodec, WireMessage,
};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn sample_node(
    node_id: i64,
    text: &str,
    resource_id: &str,
    clickable: bool,
    scrollable: bool,
    x: i32,
    y: i32,
) -> ShadowUINode {
    ShadowUINode {
        index: node_id as i32,
        text: text.to_string(),
        resource_id: resource_id.to_string(),
        content_desc: String::new(),
        class_name: "TextView".to_string(),
        clickable,
        scrollable,
        bounds: format!("[{},{},{} ,{}]", x - 50, y - 20, x + 50, y + 20).replace(" ,", ","),
        center_x: x,
        center_y: y,
        node_id,
        parent_id: None,
        window_id: 1,
        child_index: node_id as i32,
        depth: 1,
        package_name: "com.android.settings".to_string(),
        editable: false,
        focused: false,
        checked: false,
        enabled: true,
        visible: true,
    }
}

#[tokio::test]
async fn test_standardized_tool_schemas_and_http_endpoints() {
    let openai = ToolSchemaExporter::export(ToolSchemaFormat::OpenAi);
    let anthropic = ToolSchemaExporter::export(ToolSchemaFormat::Anthropic);
    let mcp = ToolSchemaExporter::export(ToolSchemaFormat::Mcp);
    let all = ToolSchemaExporter::export(ToolSchemaFormat::All);

    let oa_arr = openai.as_array().expect("openai array");
    let an_arr = anthropic.as_array().expect("anthropic array");
    let mcp_arr = mcp["tools"].as_array().expect("mcp tools array");
    assert_eq!(oa_arr.len(), 9);
    assert_eq!(an_arr.len(), 9);
    assert_eq!(mcp_arr.len(), 9);
    assert_eq!(all["tool_count"].as_u64(), Some(9));

    for item in oa_arr {
        assert_eq!(item["type"], "function");
        assert!(item["function"]["name"].as_str().unwrap().starts_with("android_"));
        assert_eq!(item["function"]["parameters"]["type"], "object");
    }
    for item in an_arr {
        assert!(item["name"].as_str().unwrap().starts_with("android_"));
        assert_eq!(item["input_schema"]["type"], "object");
    }

    // Verify android_get_ui_tree exposes opt-in hybrid vision parameters
    let ui_tree_an = an_arr
        .iter()
        .find(|t| t["name"] == "android_get_ui_tree")
        .expect("android_get_ui_tree schema");
    assert_eq!(
        ui_tree_an["input_schema"]["properties"]["include_vision"]["default"],
        json!(false)
    );
    assert_eq!(
        ui_tree_an["input_schema"]["properties"]["vision_max_width"]["default"],
        json!(540)
    );
    assert_eq!(
        ui_tree_an["input_schema"]["properties"]["vision_quality"]["default"],
        json!(50)
    );

    // Live HTTP /schemas endpoints with auth enforcement
    let tmp = std::env::temp_dir().join(format!(
        "droidpath_schemas_test_{}.json",
        uuid::Uuid::new_v4()
    ));
    let manager = DroidPathManager::new(0, tmp, None, DroidPathAdbRunner::default(), None, None);
    let core = Arc::new(DroidPathMcpServerCore::new(manager));
    let http = DroidPathHttpServer::new(core);
    let port = http
        .start(0, Some("secret-tok".to_string()))
        .await
        .expect("start http");

    async fn http_get(port: u16, path: &str, token: Option<&str>) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    let unauth = http_get(port, "/schemas/openai.json", None).await;
    assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized"));

    let oa_resp = http_get(port, "/schemas/openai.json", Some("secret-tok")).await;
    assert!(oa_resp.starts_with("HTTP/1.1 200 OK"));
    let body = oa_resp.split("\r\n\r\n").nth(1).unwrap();
    let parsed: Value = serde_json::from_str(body).unwrap();
    assert_eq!(parsed, openai);

    let an_resp = http_get(port, "/schemas/anthropic.json", Some("secret-tok")).await;
    assert!(an_resp.starts_with("HTTP/1.1 200 OK"));
    let parsed_an: Value = serde_json::from_str(an_resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(parsed_an, anthropic);

    let all_resp = http_get(port, "/schemas", Some("secret-tok")).await;
    assert!(all_resp.starts_with("HTTP/1.1 200 OK"));
    let parsed_all: Value =
        serde_json::from_str(all_resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(parsed_all["tool_count"], json!(9));

    http.stop().await;
}

#[tokio::test]
async fn test_hybrid_vision_wire_codec_and_flag_secure_blocking() {
    let codec = NdjsonWireCodec::new();
    let encoded = codec
        .encode_command(DaemonCommand::CaptureVision {
            id: 7,
            max_width: 9999,
            quality: 5,
        })
        .unwrap();
    assert_eq!(
        String::from_utf8(encoded).unwrap(),
        "{\"cmd\":\"capture_vision\",\"id\":7,\"max_width\":1080,\"quality\":10}\n"
    );

    let frame_json = br#"{"type":"vision_frame","id":7,"ok":true,"mime_type":"image/jpeg","width":540,"height":1170,"quality":50,"byte_length":6,"data_base64":"/9j/4AAQ","elapsed_ms":14,"ts_ms":1700000000000}"#;
    let decoded = codec.decode_frame(frame_json).unwrap();
    match decoded {
        WireMessage::VisionFrame(f) => {
            assert_eq!(f.id, 7);
            assert!(f.ok);
            assert_eq!(f.width, 540);
            assert_eq!(f.data_base64.as_deref(), Some("/9j/4AAQ"));
        }
        other => panic!("expected VisionFrame, got {other:?}"),
    }

    // Live mock loopback daemon supporting hybrid_vision
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let capture_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture_calls_srv = Arc::clone(&capture_calls);

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(
                b"{\"type\":\"hello\",\"protocol_version\":1,\"format\":\"ndjson/1\",\"sdk_int\":34,\"device_model\":\"Pixel\",\"pid\":100,\"capabilities\":[\"input\",\"hybrid_vision\"]}\n",
            )
            .await
            .unwrap();
        writer
            .write_all(
                b"{\"type\":\"snapshot\",\"seq\":1,\"ts_ms\":1000,\"reason\":\"initial\",\"package\":\"com.android.settings\",\"window_id\":1,\"secure_screen\":false,\"node_count\":1,\"nodes\":[{\"index\":0,\"text\":\"Display\",\"resource_id\":\"com.android.settings:id/title\",\"content_desc\":\"\",\"class\":\"TextView\",\"clickable\":true,\"scrollable\":false,\"bounds\":\"[0,100][1080,220]\",\"center_x\":540,\"center_y\":160,\"node_id\":1,\"parent_id\":null,\"window_id\":1,\"child_index\":0,\"depth\":1,\"package_name\":\"com.android.settings\",\"editable\":false,\"focused\":false,\"checked\":false,\"enabled\":true,\"visible\":true}]}\n",
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("\"capture_vision\"") {
                capture_calls_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let v: Value = serde_json::from_str(&line).unwrap();
                let id = v["id"].as_u64().unwrap();
                let resp = format!(
                    "{{\"type\":\"vision_frame\",\"id\":{id},\"ok\":true,\"mime_type\":\"image/jpeg\",\"width\":540,\"height\":1170,\"quality\":45,\"byte_length\":6,\"data_base64\":\"/9j/4AAQ\",\"elapsed_ms\":12,\"ts_ms\":1005}}\n"
                );
                writer.write_all(resp.as_bytes()).await.unwrap();
                writer.flush().await.unwrap();
            }
        }
    });

    let tmp = std::env::temp_dir().join(format!(
        "droidpath_vision_test_{}.json",
        uuid::Uuid::new_v4()
    ));
    let manager = DroidPathManager::new(port, tmp, None, DroidPathAdbRunner::default(), None, None);
    manager.ensure_started(Some("mock-serial"), false).await;
    assert!(manager.bridge.client.wait_for_hello(Duration::from_secs(2)).await);
    assert!(manager.bridge.ensure_shadow_stream_ready(2.0, 15.0).await);

    let core = DroidPathMcpServerCore::new(Arc::clone(&manager));

    // 1) Default include_vision=false returns plain node array (1 text block, no image)
    let mut args = Map::new();
    args.insert("device_serial".to_string(), json!("mock-serial"));
    let res_plain = core.execute_tool("android_get_ui_tree", &args).await;
    assert!(!res_plain.is_error);
    let plain_json = res_plain.to_json_value();
    assert_eq!(plain_json["content"].as_array().unwrap().len(), 1);
    assert_eq!(capture_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // 2) Opt-in include_vision=true returns multimodal [text, image] blocks
    let mut args_vis = Map::new();
    args_vis.insert("device_serial".to_string(), json!("mock-serial"));
    args_vis.insert("include_vision".to_string(), json!(true));
    args_vis.insert("vision_quality".to_string(), json!(45));
    let res_vis = core.execute_tool("android_get_ui_tree", &args_vis).await;
    assert!(!res_vis.is_error);
    let vis_json = res_vis.to_json_value();
    let blocks = vis_json["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["mimeType"], "image/jpeg");
    assert_eq!(blocks[1]["data"], "/9j/4AAQ");
    assert_eq!(capture_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // 3) FLAG_SECURE active strictly blocks vision capture without calling daemon
    manager
        .bridge
        .dom_state
        .handle_message(WireMessage::SecureScreen(
            droidpath_core::WireSecureScreenPayload {
                code: "SECURE_SCREEN_ACTIVE".to_string(),
                active: true,
                reason: "flag_secure".to_string(),
                active_package: "com.bank.app".to_string(),
                timestamp_ms: 2000,
            },
        ))
        .await;
    let res_sec = core.execute_tool("android_get_ui_tree", &args_vis).await;
    assert!(!res_sec.is_error);
    let sec_json = res_sec.to_json_value();
    assert_eq!(sec_json["content"].as_array().unwrap().len(), 1);
    let sec_payload: Value = serde_json::from_str(&res_sec.text).unwrap();
    assert_eq!(sec_payload["vision"]["included"], json!(false));
    assert_eq!(sec_payload["vision"]["blocked_reason"], json!("FLAG_SECURE"));
    // Still 1 — never sent capture_vision to daemon while FLAG_SECURE!
    assert_eq!(capture_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    manager.stop().await;
    server_task.abort();
}

#[test]
fn test_agentic_feedback_delta_ranking_pii_redaction_and_secure_masking() {
    let prev = ShadowTreeIndex::new(
        1,
        1000,
        "com.android.settings".to_string(),
        1,
        vec![
            sample_node(1, "Network & internet", "com.android.settings:id/net", true, false, 540, 300),
            sample_node(2, "Display", "com.android.settings:id/display", true, false, 540, 500),
        ],
    );
    let curr = ShadowTreeIndex::new(
        2,
        1100,
        "com.android.settings".to_string(),
        1,
        vec![
            sample_node(2, "Display & brightness", "com.android.settings:id/display", true, true, 540, 500),
            sample_node(3, "Night Light", "com.android.settings:id/night_light", true, false, 540, 700),
            sample_node(4, "OTP code 482910 for card 4532 0151 1283 0366", "com.android.settings:id/banner", false, false, 540, 900),
        ],
    );

    let fb = AgenticFeedbackBuilder::build(
        "element_not_found",
        Some("Night Mode"),
        Some(&prev),
        Some(&curr),
        false,
    );
    assert_eq!(fb.failure_code, "element_not_found");
    assert!(!fb.is_secure_screen);
    assert!(fb.scrollable_available);
    assert_eq!(fb.closest_candidates[0].label, "Night Light");

    let delta = fb.screen_delta.as_ref().expect("screen delta");
    assert_eq!(delta.removed_count, 1);
    assert_eq!(delta.added_count, 2);
    assert_eq!(delta.updated_count, 1);

    // Ensure zero PII leakage in candidates and changed_nodes
    let serialized = serde_json::to_string(&fb).unwrap();
    assert!(!serialized.contains("482910"), "OTP leaked in feedback: {serialized}");
    assert!(!serialized.contains("4532 0151"), "Card PAN leaked in feedback: {serialized}");

    // Ensure FLAG_SECURE suppresses all candidates & delta
    let sec_fb = AgenticFeedbackBuilder::build(
        "route_deviation",
        Some("Pay"),
        Some(&prev),
        Some(&curr),
        true,
    );
    assert!(sec_fb.is_secure_screen);
    assert!(sec_fb.closest_candidates.is_empty());
    assert!(sec_fb.screen_delta.is_none());
}

#[test]
fn test_static_schema_files_match_exporter_without_drift() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let checks = [
        ("schemas/openai-tools.json", ToolSchemaFormat::OpenAi),
        ("schemas/anthropic-tools.json", ToolSchemaFormat::Anthropic),
        ("schemas/mcp-tools.json", ToolSchemaFormat::Mcp),
        (
            "packages/droidpath-cli/assets/schemas/openai-tools.json",
            ToolSchemaFormat::OpenAi,
        ),
        (
            "packages/droidpath-cli/assets/schemas/anthropic-tools.json",
            ToolSchemaFormat::Anthropic,
        ),
        (
            "packages/droidpath-cli/assets/schemas/mcp-tools.json",
            ToolSchemaFormat::Mcp,
        ),
    ];
    for (rel, fmt) in checks {
        let path = repo_root.join(rel);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
        let on_disk: Value = serde_json::from_str(&raw).expect("valid json");
        let live = ToolSchemaExporter::export(fmt);
        assert_eq!(
            on_disk, live,
            "Static schema file {rel} drifted from ToolSchemaExporter"
        );
    }
}

#[test]
fn test_headless_ephemeral_port_allocation() {
    let p1 = DroidPathAdbRunner::allocate_ephemeral_port().expect("ephemeral port 1");
    let p2 = DroidPathAdbRunner::allocate_ephemeral_port().expect("ephemeral port 2");
    assert!(p1 >= 1024);
    assert!(p2 >= 1024);
    let _cfg = DaemonEndpointConfiguration::new("127.0.0.1", p1);
    let _client = DaemonTcpClient::new(_cfg);
}

#[tokio::test]
async fn test_http_bind_host_loopback_default_and_auth_gated_container_bind() {
    assert!(DroidPathHttpServer::is_loopback_bind_host("127.0.0.1"));
    assert!(DroidPathHttpServer::is_loopback_bind_host("::1"));
    assert!(DroidPathHttpServer::is_loopback_bind_host("[::1]"));
    assert!(DroidPathHttpServer::is_loopback_bind_host("localhost"));
    assert!(!DroidPathHttpServer::is_loopback_bind_host("0.0.0.0"));
    assert!(!DroidPathHttpServer::is_loopback_bind_host("::"));
    assert!(!DroidPathHttpServer::is_loopback_bind_host("172.17.0.2"));
    assert!(!DroidPathHttpServer::is_loopback_bind_host(""));

    let tmp = std::env::temp_dir().join(format!(
        "droidpath_bind_host_test_{}.json",
        uuid::Uuid::new_v4()
    ));
    let manager = DroidPathManager::new(0, tmp, None, DroidPathAdbRunner::default(), None, None);
    let core = Arc::new(DroidPathMcpServerCore::new(manager));
    let http = DroidPathHttpServer::new(core);

    // 1. Default start() binds strict 127.0.0.1 loopback without requiring an auth token.
    let loopback_port = http.start(0, None).await.expect("default loopback start");
    let bound = http.bound_addr().await.expect("bound addr");
    assert_eq!(bound.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    assert_eq!(bound.port(), loopback_port);

    // 2. Non-loopback bind without auth token (or whitespace-only token) is refused.
    let err_no_tok = http
        .start_on("0.0.0.0", 0, None)
        .await
        .expect_err("non-loopback without token must fail");
    assert_eq!(err_no_tok.kind(), std::io::ErrorKind::PermissionDenied);

    let err_blank_tok = http
        .start_on("0.0.0.0", 0, Some("   ".to_string()))
        .await
        .expect_err("non-loopback with blank token must fail");
    assert_eq!(err_blank_tok.kind(), std::io::ErrorKind::PermissionDenied);

    // 3. Invalid bind host strings are rejected.
    let err_empty = http
        .start_on("   ", 0, Some("tok".to_string()))
        .await
        .expect_err("empty bind host must fail");
    assert_eq!(err_empty.kind(), std::io::ErrorKind::InvalidInput);

    let err_bracket = http
        .start_on("[::1", 0, Some("tok".to_string()))
        .await
        .expect_err("malformed bracketed host must fail");
    assert_eq!(err_bracket.kind(), std::io::ErrorKind::InvalidInput);

    // 4. Auth-gated 0.0.0.0 container bind succeeds and enforces Bearer auth + Host check.
    let container_port = http
        .start_on("0.0.0.0", 0, Some("container-secret".to_string()))
        .await
        .expect("auth-gated 0.0.0.0 bind");
    let container_bound = http.bound_addr().await.expect("container bound addr");
    assert_eq!(
        container_bound.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    );
    assert_eq!(container_bound.port(), container_port);

    async fn raw_get(port: u16, host: &str, token: Option<&str>) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!("GET /health HTTP/1.1\r\nHost: {host}\r\n{auth}\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    let unauth = raw_get(container_port, &format!("127.0.0.1:{container_port}"), None).await;
    assert!(unauth.starts_with("HTTP/1.1 401 Unauthorized"));

    let bad_host = raw_get(container_port, "evil.example.com", Some("container-secret")).await;
    assert!(bad_host.starts_with("HTTP/1.1 403 Forbidden"));

    let ok = raw_get(
        container_port,
        &format!("127.0.0.1:{container_port}"),
        Some("container-secret"),
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200 OK"));

    http.stop().await;
}
