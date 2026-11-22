use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::manager::DroidPathManager;

use super::protocol::DroidPathMcpServerCore;
use super::schemas::{ToolSchemaExporter, ToolSchemaFormat};

// MARK: - Stdio Transport

pub struct DroidPathStdioTransport;

impl DroidPathStdioTransport {
    pub async fn run(server: Arc<DroidPathMcpServerCore>) -> std::io::Result<()> {
        DroidPathManager::log_stderr(&format!(
            "droidpath-mcp stdio server started (protocol {})",
            DroidPathMcpServerCore::PROTOCOL_VERSION
        ));
        server.manager.load_persisted_graph_if_needed().await;

        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(mut resp) = server.handle_raw_request_data(trimmed.as_bytes()).await {
                resp.push(b'\n');
                stdout.write_all(&resp).await?;
                stdout.flush().await?;
            }
        }

        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(2500),
            server.manager.stop(),
        )
        .await;
        DroidPathManager::log_stderr("droidpath-mcp stdio server exiting on EOF");
        Ok(())
    }
}

// MARK: - Loopback HTTP/SSE Transport

pub struct DroidPathHttpServer {
    server: Arc<DroidPathMcpServerCore>,
    sse_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    accept_task: Mutex<Option<JoinHandle<()>>>,
    bound_addr: Mutex<Option<std::net::SocketAddr>>,
    bound_port: Mutex<u16>,
}

impl DroidPathHttpServer {
    const MAX_HEADER_BYTES: usize = 32_768;
    const MAX_BODY_BYTES: usize = 1_048_576;

    pub fn new(server: Arc<DroidPathMcpServerCore>) -> Self {
        Self {
            server,
            sse_sessions: Arc::new(Mutex::new(HashMap::new())),
            accept_task: Mutex::new(None),
            bound_addr: Mutex::new(None),
            bound_port: Mutex::new(0),
        }
    }

    pub async fn port(&self) -> u16 {
        *self.bound_port.lock().await
    }

    pub async fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        *self.bound_addr.lock().await
    }

    pub fn is_loopback_bind_host(raw_host: &str) -> bool {
        let trimmed = raw_host.trim();
        if trimmed.is_empty() {
            return false;
        }
        let clean = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };
        if clean.eq_ignore_ascii_case("localhost") {
            return true;
        }
        clean
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }

    pub async fn start(
        &self,
        port: u16,
        require_auth_token: Option<String>,
    ) -> std::io::Result<u16> {
        self.start_on("127.0.0.1", port, require_auth_token).await
    }

    pub async fn start_on(
        &self,
        bind_host: &str,
        port: u16,
        require_auth_token: Option<String>,
    ) -> std::io::Result<u16> {
        let trimmed_host = bind_host.trim();
        if trimmed_host.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "HTTP bind host must not be empty",
            ));
        }
        let clean_host = if trimmed_host.starts_with('[') || trimmed_host.ends_with(']') {
            if trimmed_host.starts_with('[')
                && trimmed_host.ends_with(']')
                && trimmed_host.len() > 2
            {
                &trimmed_host[1..trimmed_host.len() - 1]
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("Malformed bracketed bind host: '{trimmed_host}'"),
                ));
            }
        } else {
            trimmed_host
        };

        let auth_token = require_auth_token
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_string();

        if !Self::is_loopback_bind_host(clean_host) && auth_token.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Refusing to bind non-loopback address '{clean_host}' without a non-empty --auth-token"
                ),
            ));
        }

        self.stop().await;

        let listener = TcpListener::bind((clean_host, port)).await?;
        let local_addr = listener.local_addr()?;
        if !local_addr.ip().is_loopback() && auth_token.is_empty() {
            drop(listener);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Refusing to bind resolved non-loopback address '{}' without a non-empty --auth-token",
                    local_addr.ip()
                ),
            ));
        }
        let actual_port = local_addr.port();
        *self.bound_addr.lock().await = Some(local_addr);
        *self.bound_port.lock().await = actual_port;

        let server = Arc::clone(&self.server);
        let sessions = Arc::clone(&self.sse_sessions);

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let srv = Arc::clone(&server);
                let sess = Arc::clone(&sessions);
                let tok = auth_token.clone();
                tokio::spawn(async move {
                    Self::handle_connection(stream, srv, sess, tok).await;
                });
            }
        });

        *self.accept_task.lock().await = Some(handle);
        DroidPathManager::log_stderr(&format!(
            "droidpath-mcp HTTP/SSE server listening on http://{local_addr}/sse"
        ));
        Ok(actual_port)
    }

    pub async fn stop(&self) {
        if let Some(task) = self.accept_task.lock().await.take() {
            task.abort();
        }
        *self.bound_addr.lock().await = None;
        *self.bound_port.lock().await = 0;
        self.sse_sessions.lock().await.clear();
    }

    pub fn is_allowed_loopback_host(raw_host: Option<&str>) -> bool {
        let Some(raw) = raw_host else {
            return false;
        };
        if raw.chars().any(|c| c.is_control()) {
            return false;
        }
        let trimmed = raw.trim().to_ascii_lowercase();
        if trimmed.is_empty()
            || trimmed
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '@' | '/' | '?' | '#' | '\\'))
        {
            return false;
        }
        let host = if let Some(rest) = trimmed.strip_prefix('[') {
            match rest.split_once(']') {
                Some((inner, tail)) => {
                    if !tail.is_empty() {
                        let Some(port_str) = tail.strip_prefix(':') else {
                            return false;
                        };
                        if port_str.is_empty()
                            || !port_str.chars().all(|c| c.is_ascii_digit())
                            || port_str.parse::<u16>().ok().filter(|&p| p > 0).is_none()
                        {
                            return false;
                        }
                    }
                    inner
                }
                None => return false,
            }
        } else if let Some((h, port_str)) = trimmed.split_once(':') {
            if port_str.is_empty()
                || !port_str.chars().all(|c| c.is_ascii_digit())
                || port_str.parse::<u16>().ok().filter(|&p| p > 0).is_none()
            {
                return false;
            }
            h
        } else {
            trimmed.as_str()
        };
        matches!(host, "127.0.0.1" | "localhost" | "::1")
    }

    pub fn is_allowed_origin(raw_origin: Option<&str>) -> bool {
        let Some(raw) = raw_origin else {
            return true;
        };
        if raw.chars().any(|c| c.is_control()) {
            return false;
        }
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '@' | '?' | '#' | '\\'))
        {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        let rest = if let Some(r) = lower.strip_prefix("http://") {
            r
        } else if let Some(r) = lower.strip_prefix("https://") {
            r
        } else {
            return false;
        };
        // Strict RFC 6454 Origin: scheme://host[:port] only (zero path, query, fragment, or userinfo).
        if rest.contains('/') {
            return false;
        }
        Self::is_allowed_loopback_host(Some(rest))
    }

    fn constant_time_equals(lhs: &str, rhs: &str) -> bool {
        let a = lhs.as_bytes();
        let b = rhs.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (&x, &y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    fn extract_query_param(key: &str, raw_target: &str) -> Option<String> {
        let (_, query) = raw_target.split_once('?')?;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == key {
                    return Some(v.to_string());
                }
            }
        }
        None
    }

    fn decode_chunked_body(raw: &[u8], max_bytes: usize) -> Result<Option<Vec<u8>>, &'static str> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            let Some(rel_crlf) = raw[pos..].windows(2).position(|w| w == b"\r\n") else {
                return Ok(None); // Need more bytes
            };
            let size_line = std::str::from_utf8(&raw[pos..pos + rel_crlf])
                .map_err(|_| "Invalid chunk size header")?;
            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let chunk_size =
                usize::from_str_radix(size_hex, 16).map_err(|_| "Malformed chunk size")?;
            pos += rel_crlf + 2;
            if chunk_size == 0 {
                // Consume trailing CRLF (or trailer headers ending in \r\n\r\n)
                if raw[pos..].starts_with(b"\r\n") {
                    return Ok(Some(out));
                }
                if let Some(trailer_end) = raw[pos..].windows(4).position(|w| w == b"\r\n\r\n") {
                    let _ = trailer_end;
                    return Ok(Some(out));
                }
                return Ok(None);
            }
            if out.len().saturating_add(chunk_size) > max_bytes {
                return Err("Payload Too Large");
            }
            if raw.len() < pos + chunk_size + 2 {
                return Ok(None); // Need more bytes
            }
            out.extend_from_slice(&raw[pos..pos + chunk_size]);
            if &raw[pos + chunk_size..pos + chunk_size + 2] != b"\r\n" {
                return Err("Missing CRLF after chunk payload");
            }
            pos += chunk_size + 2;
        }
    }

    async fn handle_connection(
        mut stream: TcpStream,
        server: Arc<DroidPathMcpServerCore>,
        sessions: Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
        auth_token: String,
    ) {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 8192];
        let header_end;
        let read_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

        loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = pos;
                break;
            }
            if buf.len() >= Self::MAX_HEADER_BYTES {
                return;
            }
            let remaining = read_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            }
        }

        let header_text = String::from_utf8_lossy(&buf[..header_end]);
        let mut lines = header_text.split("\r\n");
        let Some(request_line) = lines.next() else {
            return;
        };
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0].to_ascii_uppercase();
        let raw_target = parts[1].to_string();
        let target_path = raw_target
            .split_once('?')
            .map(|(p, _)| p)
            .unwrap_or(&raw_target)
            .to_string();

        let mut headers = HashMap::new();
        let mut host_headers: Vec<String> = Vec::new();
        let mut origin_headers: Vec<String> = Vec::new();
        let mut content_length_headers: Vec<String> = Vec::new();
        for line in lines {
            if line.contains('\r') || line.contains('\n') || line.contains('\0') {
                Self::send_http_response(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    b"Malformed header",
                    None,
                )
                .await;
                return;
            }
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                match key.as_str() {
                    "host" => host_headers.push(val.clone()),
                    "origin" => origin_headers.push(val.clone()),
                    "content-length" => content_length_headers.push(val.clone()),
                    _ => {}
                }
                headers.insert(key, val);
            }
        }

        let host_ok = host_headers.len() == 1
            && Self::is_allowed_loopback_host(host_headers.first().map(String::as_str));
        let origin_ok = match origin_headers.len() {
            0 => true,
            1 => Self::is_allowed_origin(origin_headers.first().map(String::as_str)),
            _ => false,
        };

        if !host_ok || !origin_ok {
            Self::send_http_response(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                b"Forbidden host/origin",
                None,
            )
            .await;
            return;
        }

        let allowed_origin = origin_headers.first().map(String::as_str);

        if method == "OPTIONS" {
            Self::send_http_response(&mut stream, "204 No Content", "text/plain", b"", allowed_origin)
                .await;
            return;
        }

        if !auth_token.is_empty() {
            let auth_header = headers
                .get("authorization")
                .map(String::as_str)
                .unwrap_or("");
            let provided = if auth_header.len() >= 7
                && auth_header[..7].eq_ignore_ascii_case("bearer ")
            {
                auth_header[7..].trim()
            } else {
                Self::send_http_response(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    b"Unauthorized",
                    allowed_origin,
                )
                .await;
                return;
            };
            if !Self::constant_time_equals(provided, &auth_token) {
                Self::send_http_response(
                    &mut stream,
                    "401 Unauthorized",
                    "text/plain",
                    b"Unauthorized",
                    allowed_origin,
                )
                .await;
                return;
            }
        }

        let is_chunked = headers
            .get("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false);
        let body_start = header_end + 4;
        let owned_body: Vec<u8>;

        if is_chunked {
            loop {
                match Self::decode_chunked_body(&buf[body_start..], Self::MAX_BODY_BYTES) {
                    Ok(Some(decoded)) => {
                        owned_body = decoded;
                        break;
                    }
                    Ok(None) => {
                        if buf.len() - body_start > Self::MAX_BODY_BYTES + 65_536 {
                            Self::send_http_response(
                                &mut stream,
                                "413 Payload Too Large",
                                "text/plain",
                                b"Payload Too Large",
                                allowed_origin,
                            )
                            .await;
                            return;
                        }
                        let remaining =
                            read_deadline.saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            Self::send_http_response(
                                &mut stream,
                                "408 Request Timeout",
                                "text/plain",
                                b"Request Timeout",
                                allowed_origin,
                            )
                            .await;
                            return;
                        }
                        match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
                            Ok(Ok(0)) => {
                                Self::send_http_response(
                                    &mut stream,
                                    "400 Bad Request",
                                    "text/plain",
                                    b"Incomplete chunked body",
                                    allowed_origin,
                                )
                                .await;
                                return;
                            }
                            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                            Ok(Err(_)) | Err(_) => return,
                        }
                    }
                    Err("Payload Too Large") => {
                        Self::send_http_response(
                            &mut stream,
                            "413 Payload Too Large",
                            "text/plain",
                            b"Payload Too Large",
                            allowed_origin,
                        )
                        .await;
                        return;
                    }
                    Err(msg) => {
                        Self::send_http_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            msg.as_bytes(),
                            allowed_origin,
                        )
                        .await;
                        return;
                    }
                }
            }
        } else {
            let mut parsed_cl: Option<usize> = None;
            for raw_cl in &content_length_headers {
                let trimmed = raw_cl.trim();
                if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
                    Self::send_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        b"Invalid Content-Length",
                        allowed_origin,
                    )
                    .await;
                    return;
                }
                let Ok(u) = trimmed.parse::<usize>() else {
                    Self::send_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        b"Invalid Content-Length",
                        allowed_origin,
                    )
                    .await;
                    return;
                };
                if u > Self::MAX_BODY_BYTES {
                    Self::send_http_response(
                        &mut stream,
                        "413 Payload Too Large",
                        "text/plain",
                        b"Payload Too Large",
                        allowed_origin,
                    )
                    .await;
                    return;
                }
                if let Some(prev) = parsed_cl {
                    if prev != u {
                        Self::send_http_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            b"Conflicting Content-Length",
                            allowed_origin,
                        )
                        .await;
                        return;
                    }
                } else {
                    parsed_cl = Some(u);
                }
            }
            let content_length: usize = parsed_cl.unwrap_or(0);

            while buf.len() - body_start < content_length {
                let remaining =
                    read_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    Self::send_http_response(
                        &mut stream,
                        "408 Request Timeout",
                        "text/plain",
                        b"Request Timeout",
                        allowed_origin,
                    )
                    .await;
                    return;
                }
                match tokio::time::timeout(remaining, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) => {
                        Self::send_http_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            b"Incomplete request body",
                            allowed_origin,
                        )
                        .await;
                        return;
                    }
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    Ok(Err(_)) | Err(_) => return,
                }
            }
            owned_body = buf[body_start..body_start + content_length].to_vec();
        }

        let body_data: &[u8] = &owned_body;

        if method == "GET" && (target_path == "/sse" || target_path == "/") {
            let session_id = Uuid::new_v4().to_string();
            let cors_headers = Self::loopback_cors_headers(allowed_origin);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\n{cors_headers}Connection: keep-alive\r\n\r\nevent: endpoint\ndata: /message?sessionId={session_id}\n\n"
            );
            if stream.write_all(header.as_bytes()).await.is_err()
                || stream.flush().await.is_err()
            {
                return;
            }
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
            sessions.lock().await.insert(session_id.clone(), tx);

            let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(25));
            keepalive.tick().await;
            loop {
                tokio::select! {
                    maybe_msg = rx.recv() => {
                        let Some(msg) = maybe_msg else { break; };
                        if stream.write_all(&msg).await.is_err() || stream.flush().await.is_err() {
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if stream.write_all(b": keepalive\n\n").await.is_err() || stream.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
            sessions.lock().await.remove(&session_id);
            return;
        }

        if method == "GET" && target_path == "/health" {
            let health = format!(
                "{{\"server\":\"droidpath-mcp\",\"version\":\"{}\",\"protocol\":\"{}\",\"status\":\"running\"}}",
                DroidPathMcpServerCore::SERVER_VERSION,
                DroidPathMcpServerCore::PROTOCOL_VERSION
            );
            Self::send_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                health.as_bytes(),
                allowed_origin,
            )
            .await;
            return;
        }

        if method == "GET"
            && matches!(
                target_path.as_str(),
                "/schemas" | "/schemas/openai.json" | "/schemas/anthropic.json" | "/schemas/mcp.json"
            )
        {
            let format = match target_path.as_str() {
                "/schemas/openai.json" => ToolSchemaFormat::OpenAi,
                "/schemas/anthropic.json" => ToolSchemaFormat::Anthropic,
                "/schemas/mcp.json" => ToolSchemaFormat::Mcp,
                _ => ToolSchemaFormat::All,
            };
            let body = serde_json::to_vec_pretty(&ToolSchemaExporter::export(format))
                .unwrap_or_else(|_| b"{}".to_vec());
            Self::send_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                &body,
                allowed_origin,
            )
            .await;
            return;
        }

        if method == "POST" && (target_path == "/message" || target_path == "/") {
            let session_id = Self::extract_query_param("sessionId", &raw_target);
            let resp_data = server.handle_raw_request_data(body_data).await;
            if let Some(ref sid) = session_id {
                let maybe_tx = sessions.lock().await.get(sid).cloned();
                if let (Some(tx), Some(ref data)) = (maybe_tx, &resp_data) {
                    let json_str = String::from_utf8_lossy(data);
                    let sse_event = format!("event: message\ndata: {json_str}\n\n");
                    if tx.send(sse_event.into_bytes()).await.is_err() {
                        sessions.lock().await.remove(sid);
                    }
                    Self::send_http_response(
                        &mut stream,
                        "202 Accepted",
                        "application/json",
                        b"{\"ok\":true}",
                        allowed_origin,
                    )
                    .await;
                    return;
                }
            }
            if let Some(data) = resp_data {
                Self::send_http_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &data,
                    allowed_origin,
                )
                .await;
            } else {
                Self::send_http_response(
                    &mut stream,
                    "202 Accepted",
                    "application/json",
                    b"",
                    allowed_origin,
                )
                .await;
            }
            return;
        }

        Self::send_http_response(
            &mut stream,
            "404 Not Found",
            "text/plain",
            b"Not Found",
            allowed_origin,
        )
        .await;
    }

    fn loopback_cors_headers(allowed_origin: Option<&str>) -> String {
        match allowed_origin {
            Some(origin)
                if !origin.is_empty() && !origin.chars().any(|c| c.is_control()) =>
            {
                format!(
                    "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nVary: Origin\r\n"
                )
            }
            _ => String::new(),
        }
    }

    async fn send_http_response(
        stream: &mut TcpStream,
        status: &str,
        content_type: &str,
        body: &[u8],
        allowed_origin: Option<&str>,
    ) {
        let cors_headers = Self::loopback_cors_headers(allowed_origin);
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\n{cors_headers}Connection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes()).await;
        if !body.is_empty() {
            let _ = stream.write_all(body).await;
        }
        let _ = stream.flush().await;
    }
}
