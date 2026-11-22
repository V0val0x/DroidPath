use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::protocol::{
    DaemonCommand, NdjsonWireCodec, WireCodec, WireInputResultPayload, WireMessage,
    WireVisionFramePayload,
};

use super::endpoint::{
    DaemonClientError, DaemonClientMetrics, DaemonConnectionState, DaemonEndpointConfiguration,
};
use super::framer::LineFramer;

pub type BeforeReconnectHook =
    Arc<dyn Fn(usize) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

struct ClientInner {
    state: DaemonConnectionState,
    metrics: DaemonClientMetrics,
    is_stopped: bool,
    generation: u64,
    reconnect_attempt: usize,
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    loop_task: Option<JoinHandle<()>>,
    before_reconnect: Option<BeforeReconnectHook>,
}

type InputWaiter = oneshot::Sender<Result<WireInputResultPayload, DaemonClientError>>;
type VisionWaiter = oneshot::Sender<Result<WireVisionFramePayload, DaemonClientError>>;

/// Persistent cross-platform Tokio TCP client for `android-daemon` (`127.0.0.1:27189`).
/// Preserves strict TCP chunk framing order, detects silent dead connections via heartbeat
/// watchdog, and reconnects with jittered exponential backoff.
pub struct DaemonTcpClient {
    config: DaemonEndpointConfiguration,
    codec: Arc<dyn WireCodec>,
    inner: Arc<Mutex<ClientInner>>,
    state_watch: Arc<RwLock<DaemonConnectionState>>,
    msg_tx: broadcast::Sender<WireMessage>,
    state_tx: broadcast::Sender<DaemonConnectionState>,
    next_input_id: AtomicU64,
    pending_inputs: Arc<std::sync::Mutex<HashMap<u64, InputWaiter>>>,
    pending_vision: Arc<std::sync::Mutex<HashMap<u64, VisionWaiter>>>,
}

impl DaemonTcpClient {
    pub fn new(config: DaemonEndpointConfiguration) -> Self {
        Self::with_codec(config, Arc::new(NdjsonWireCodec::new()))
    }

    pub fn with_codec(config: DaemonEndpointConfiguration, codec: Arc<dyn WireCodec>) -> Self {
        let (msg_tx, _) = broadcast::channel(512);
        let (state_tx, _) = broadcast::channel(64);
        Self {
            config,
            codec,
            inner: Arc::new(Mutex::new(ClientInner {
                state: DaemonConnectionState::Disconnected,
                metrics: DaemonClientMetrics::default(),
                is_stopped: false,
                generation: 0,
                reconnect_attempt: 0,
                writer_tx: None,
                loop_task: None,
                before_reconnect: None,
            })),
            state_watch: Arc::new(RwLock::new(DaemonConnectionState::Disconnected)),
            msg_tx,
            state_tx,
            next_input_id: AtomicU64::new(1),
            pending_inputs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_vision: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn configuration(&self) -> &DaemonEndpointConfiguration {
        &self.config
    }

    pub async fn state(&self) -> DaemonConnectionState {
        self.state_watch.read().await.clone()
    }

    pub async fn metrics(&self) -> DaemonClientMetrics {
        self.inner.lock().await.metrics.clone()
    }

    pub fn subscribe_messages(&self) -> broadcast::Receiver<WireMessage> {
        self.msg_tx.subscribe()
    }

    pub fn subscribe_states(&self) -> broadcast::Receiver<DaemonConnectionState> {
        self.state_tx.subscribe()
    }

    pub async fn set_before_reconnect_hook(&self, hook: Option<BeforeReconnectHook>) {
        self.inner.lock().await.before_reconnect = hook;
    }

    pub async fn start(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        match inner.state {
            DaemonConnectionState::Connecting | DaemonConnectionState::Connected { .. } => return,
            DaemonConnectionState::Disconnected
            | DaemonConnectionState::WaitingToReconnect { .. }
            | DaemonConnectionState::Stopped => {}
        }
        if let Some(handle) = inner.loop_task.take() {
            handle.abort();
        }
        inner.is_stopped = false;
        inner.reconnect_attempt = 0;
        inner.generation = inner.generation.wrapping_add(1);
        let gen = inner.generation;
        if self.config.port != 0 {
            inner.state = DaemonConnectionState::Connecting;
        }
        drop(inner);

        if self.config.port != 0 {
            *self.state_watch.write().await = DaemonConnectionState::Connecting;
            let _ = self.state_tx.send(DaemonConnectionState::Connecting);
        }

        let client = Arc::clone(self);
        let handle = tokio::spawn(async move {
            client.connection_loop(gen, false).await;
        });
        self.inner.lock().await.loop_task = Some(handle);
    }

    pub async fn reconnect(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        if inner.is_stopped {
            return;
        }
        if let Some(handle) = inner.loop_task.take() {
            handle.abort();
        }
        inner.writer_tx = None;
        inner.reconnect_attempt = 0;
        inner.generation = inner.generation.wrapping_add(1);
        let gen = inner.generation;
        if self.config.port != 0 {
            inner.state = DaemonConnectionState::Connecting;
        }
        drop(inner);

        self.fail_pending_inputs(DaemonClientError::NotConnected)
            .await;

        if self.config.port != 0 {
            *self.state_watch.write().await = DaemonConnectionState::Connecting;
            let _ = self.state_tx.send(DaemonConnectionState::Connecting);
        }

        let client = Arc::clone(self);
        let handle = tokio::spawn(async move {
            client.connection_loop(gen, true).await;
        });
        self.inner.lock().await.loop_task = Some(handle);
    }

    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        inner.is_stopped = true;
        inner.generation = inner.generation.wrapping_add(1);
        inner.writer_tx = None;
        if let Some(handle) = inner.loop_task.take() {
            handle.abort();
        }
        inner.state = DaemonConnectionState::Stopped;
        drop(inner);

        self.fail_pending_inputs(DaemonClientError::Stopped).await;
        *self.state_watch.write().await = DaemonConnectionState::Stopped;
        let _ = self.state_tx.send(DaemonConnectionState::Stopped);
    }

    pub async fn send(&self, command: DaemonCommand) -> Result<(), DaemonClientError> {
        let writer_tx = {
            let inner = self.inner.lock().await;
            if inner.is_stopped {
                return Err(DaemonClientError::Stopped);
            }
            if !inner.state.is_connected() {
                return Err(DaemonClientError::NotConnected);
            }
            inner.writer_tx.clone()
        };
        let Some(tx) = writer_tx else {
            return Err(DaemonClientError::NotConnected);
        };
        let payload = self
            .codec
            .encode_command(command)
            .map_err(|e| DaemonClientError::SendFailed(e.to_string()))?;
        tx.send(payload)
            .await
            .map_err(|e| DaemonClientError::SendFailed(e.to_string()))
    }

    pub async fn request_snapshot(&self) -> Result<(), DaemonClientError> {
        self.send(DaemonCommand::Snapshot).await
    }

    pub async fn ping(&self) -> Result<(), DaemonClientError> {
        self.send(DaemonCommand::Ping).await
    }

    pub async fn set_debounce(&self, ms: i32) -> Result<(), DaemonClientError> {
        self.send(DaemonCommand::SetDebounce { ms }).await
    }

    pub async fn supports_native_input(&self) -> bool {
        matches!(
            &*self.state_watch.read().await,
            DaemonConnectionState::Connected { hello: Some(h) } if h.supports_native_input()
        )
    }

    pub async fn supports_hybrid_vision(&self) -> bool {
        matches!(
            &*self.state_watch.read().await,
            DaemonConnectionState::Connected { hello: Some(h) } if h.supports_hybrid_vision()
        )
    }

    /// Waits briefly for an in-progress handshake (`Connecting` or `Connected { hello: None }`)
    /// to deliver `WireHello` so cold-start MCP calls can use native TCP input instead of
    /// falling back to ADB while `hello` is still in flight. Returns immediately if already
    /// ready or if the client is disconnected/waiting to reconnect/stopped.
    pub async fn wait_for_hello(&self, max_wait: Duration) -> bool {
        let mut rx = self.state_tx.subscribe();
        match &*self.state_watch.read().await {
            DaemonConnectionState::Connected { hello: Some(_) } => return true,
            DaemonConnectionState::Disconnected
            | DaemonConnectionState::WaitingToReconnect { .. }
            | DaemonConnectionState::Stopped => return false,
            DaemonConnectionState::Connecting
            | DaemonConnectionState::Connected { hello: None } => {}
        }
        if max_wait.is_zero() {
            return false;
        }
        let deadline = tokio::time::sleep(max_wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    return matches!(
                        &*self.state_watch.read().await,
                        DaemonConnectionState::Connected { hello: Some(_) }
                    );
                }
                res = rx.recv() => {
                    match res {
                        Ok(DaemonConnectionState::Connected { hello: Some(_) }) => return true,
                        Ok(DaemonConnectionState::Disconnected)
                        | Ok(DaemonConnectionState::WaitingToReconnect { .. })
                        | Ok(DaemonConnectionState::Stopped) => return false,
                        Ok(DaemonConnectionState::Connecting)
                        | Ok(DaemonConnectionState::Connected { hello: None }) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            match &*self.state_watch.read().await {
                                DaemonConnectionState::Connected { hello: Some(_) } => return true,
                                DaemonConnectionState::Disconnected
                                | DaemonConnectionState::WaitingToReconnect { .. }
                                | DaemonConnectionState::Stopped => return false,
                                _ => continue,
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => return false,
                    }
                }
            }
        }
    }

    pub fn pending_input_count(&self) -> usize {
        self.pending_inputs
            .lock()
            .map(|g| g.len())
            .unwrap_or(0)
    }

    pub async fn inject_tap(
        &self,
        x: i32,
        y: i32,
    ) -> Result<WireInputResultPayload, DaemonClientError> {
        let id = self.next_input_id.fetch_add(1, Ordering::Relaxed);
        self.send_input_and_await(id, DaemonCommand::Tap { id, x, y }, Duration::from_millis(1500))
            .await
    }

    pub async fn inject_swipe(
        &self,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: i32,
        hold_ms: i32,
    ) -> Result<WireInputResultPayload, DaemonClientError> {
        let id = self.next_input_id.fetch_add(1, Ordering::Relaxed);
        let timeout_ms = ((duration_ms.max(0) as u64) + (hold_ms.max(0) as u64) + 1500).max(2000);
        self.send_input_and_await(
            id,
            DaemonCommand::Swipe {
                id,
                x1,
                y1,
                x2,
                y2,
                duration_ms,
                hold_ms,
            },
            Duration::from_millis(timeout_ms),
        )
        .await
    }

    pub async fn inject_keyevent(
        &self,
        keycode: i32,
    ) -> Result<WireInputResultPayload, DaemonClientError> {
        let id = self.next_input_id.fetch_add(1, Ordering::Relaxed);
        self.send_input_and_await(
            id,
            DaemonCommand::Keyevent { id, keycode },
            Duration::from_millis(1500),
        )
        .await
    }

    pub async fn request_vision_frame(
        &self,
        max_width: i32,
        quality: i32,
        timeout: Duration,
    ) -> Result<WireVisionFramePayload, DaemonClientError> {
        if !self.supports_hybrid_vision().await {
            let state = self.state_watch.read().await.clone();
            return Err(match state {
                DaemonConnectionState::Stopped => DaemonClientError::Stopped,
                DaemonConnectionState::Connected { .. } => DaemonClientError::NotSupported,
                _ => DaemonClientError::NotConnected,
            });
        }

        struct PendingVisionGuard<'a> {
            pending: &'a std::sync::Mutex<HashMap<u64, VisionWaiter>>,
            id: u64,
        }
        impl Drop for PendingVisionGuard<'_> {
            fn drop(&mut self) {
                if let Ok(mut guard) = self.pending.lock() {
                    guard.remove(&self.id);
                }
            }
        }

        let id = self.next_input_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut guard) = self.pending_vision.lock() {
            guard.insert(id, tx);
        }
        let _cleanup_guard = PendingVisionGuard {
            pending: &self.pending_vision,
            id,
        };

        self.send(DaemonCommand::CaptureVision {
            id,
            max_width: max_width.clamp(160, 1080),
            quality: quality.clamp(10, 85),
        })
        .await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(frame))) => Ok(frame),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(_)) => Err(DaemonClientError::NotConnected),
            Err(_) => Err(DaemonClientError::InputTimeout),
        }
    }

    async fn send_input_and_await(
        &self,
        id: u64,
        command: DaemonCommand,
        timeout: Duration,
    ) -> Result<WireInputResultPayload, DaemonClientError> {
        if !self.supports_native_input().await {
            let state = self.state_watch.read().await.clone();
            return Err(match state {
                DaemonConnectionState::Stopped => DaemonClientError::Stopped,
                DaemonConnectionState::Connected { .. } => DaemonClientError::NotSupported,
                _ => DaemonClientError::NotConnected,
            });
        }

        struct PendingInputGuard<'a> {
            pending: &'a std::sync::Mutex<HashMap<u64, InputWaiter>>,
            id: u64,
        }
        impl Drop for PendingInputGuard<'_> {
            fn drop(&mut self) {
                if let Ok(mut guard) = self.pending.lock() {
                    guard.remove(&self.id);
                }
            }
        }

        let (tx, rx) = oneshot::channel();
        // Race-free order: register waiter BEFORE writing command to socket.
        if let Ok(mut guard) = self.pending_inputs.lock() {
            guard.insert(id, tx);
        }
        let _cleanup_guard = PendingInputGuard {
            pending: &self.pending_inputs,
            id,
        };

        self.send(command).await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => {
                if result.ok {
                    Ok(result)
                } else {
                    Err(DaemonClientError::InputFailed(
                        result
                            .error
                            .unwrap_or_else(|| "daemon_input_returned_false".to_string()),
                    ))
                }
            }
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(_)) => Err(DaemonClientError::NotConnected),
            Err(_) => Err(DaemonClientError::InputTimeout),
        }
    }

    async fn fail_pending_inputs(&self, err: DaemonClientError) {
        let waiters: Vec<InputWaiter> = match self.pending_inputs.lock() {
            Ok(mut guard) => guard.drain().map(|(_, tx)| tx).collect(),
            Err(poisoned) => poisoned.into_inner().drain().map(|(_, tx)| tx).collect(),
        };
        let clone_err = |e: &DaemonClientError| match e {
            DaemonClientError::Stopped => DaemonClientError::Stopped,
            DaemonClientError::NotConnected => DaemonClientError::NotConnected,
            DaemonClientError::NotSupported => DaemonClientError::NotSupported,
            DaemonClientError::InputTimeout => DaemonClientError::InputTimeout,
            DaemonClientError::SendFailed(s) => DaemonClientError::SendFailed(s.clone()),
            DaemonClientError::InputFailed(s) => DaemonClientError::InputFailed(s.clone()),
            DaemonClientError::InvalidEndpoint { host, port } => {
                DaemonClientError::InvalidEndpoint {
                    host: host.clone(),
                    port: *port,
                }
            }
        };
        for tx in waiters {
            let _ = tx.send(Err(clone_err(&err)));
        }
        let vision_waiters: Vec<VisionWaiter> = match self.pending_vision.lock() {
            Ok(mut guard) => guard.drain().map(|(_, tx)| tx).collect(),
            Err(poisoned) => poisoned.into_inner().drain().map(|(_, tx)| tx).collect(),
        };
        for tx in vision_waiters {
            let _ = tx.send(Err(clone_err(&err)));
        }
    }

    async fn set_state(&self, gen: u64, new_state: DaemonConnectionState) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.generation != gen || inner.is_stopped {
            return false;
        }
        let changed = inner.state != new_state;
        inner.state = new_state.clone();
        drop(inner);

        *self.state_watch.write().await = new_state.clone();
        if changed {
            let _ = self.state_tx.send(new_state);
        }
        true
    }

    async fn connection_loop(self: Arc<Self>, gen: u64, run_hook_immediately: bool) {
        if run_hook_immediately {
            let hook = self.inner.lock().await.before_reconnect.clone();
            if let Some(hook) = hook {
                hook(0).await;
            }
        }

        loop {
            {
                let inner = self.inner.lock().await;
                if inner.generation != gen || inner.is_stopped {
                    return;
                }
            }

            if self.config.port == 0 {
                let err_msg = "Invalid port 0".to_string();
                if !self.wait_before_reconnect(gen, Some(err_msg)).await {
                    return;
                }
                continue;
            }

            if !self
                .set_state(gen, DaemonConnectionState::Connecting)
                .await
            {
                return;
            }

            let addr = format!("{}:{}", self.config.host, self.config.port);
            let connect_res = tokio::time::timeout(
                Duration::from_secs(4),
                TcpStream::connect(&addr),
            )
            .await;

            let stream = match connect_res {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    if !self
                        .wait_before_reconnect(gen, Some(err.to_string()))
                        .await
                    {
                        return;
                    }
                    continue;
                }
                Err(_) => {
                    if !self
                        .wait_before_reconnect(gen, Some("Connection timed out".to_string()))
                        .await
                    {
                        return;
                    }
                    continue;
                }
            };

            let _ = stream.set_nodelay(true);
            let (mut reader, mut writer) = stream.into_split();
            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

            {
                let mut inner = self.inner.lock().await;
                if inner.generation != gen || inner.is_stopped {
                    return;
                }
                inner.writer_tx = Some(tx);
            }

            if !self
                .set_state(gen, DaemonConnectionState::Connected { hello: None })
                .await
            {
                return;
            }

            let writer_task = tokio::spawn(async move {
                while let Some(bytes) = rx.recv().await {
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
            });

            let disconnect_reason = self.read_session(gen, &mut reader).await;
            writer_task.abort();
            self.fail_pending_inputs(DaemonClientError::NotConnected)
                .await;

            {
                let mut inner = self.inner.lock().await;
                if inner.generation == gen {
                    inner.writer_tx = None;
                } else {
                    return;
                }
            }

            if !self.wait_before_reconnect(gen, disconnect_reason).await {
                return;
            }
        }
    }

    async fn read_session(
        &self,
        gen: u64,
        reader: &mut tokio::net::tcp::OwnedReadHalf,
    ) -> Option<String> {
        let mut framer = LineFramer::new(self.config.max_frame_bytes);
        let mut buf = vec![0u8; 65_536];
        let heartbeat_timeout = Duration::from_secs_f64(self.config.heartbeat_timeout_secs.max(0.5));
        let mut last_activity = Instant::now();

        loop {
            let remaining = heartbeat_timeout.saturating_sub(last_activity.elapsed());
            if remaining.is_zero() {
                return Some(format!(
                    "Heartbeat silence timeout ({:.1}s)",
                    last_activity.elapsed().as_secs_f64()
                ));
            }

            let read_res = tokio::time::timeout(remaining, reader.read(&mut buf)).await;
            let n = match read_res {
                Ok(Ok(0)) => return Some("Connection closed by remote".to_string()),
                Ok(Ok(n)) => n,
                Ok(Err(err)) => return Some(err.to_string()),
                Err(_) => {
                    return Some(format!(
                        "Heartbeat silence timeout ({:.1}s)",
                        last_activity.elapsed().as_secs_f64()
                    ));
                }
            };

            last_activity = Instant::now();
            let frames = match framer.append(&buf[..n]) {
                Ok(frames) => frames,
                Err(err) => return Some(format!("Frame error: {err}")),
            };

            let mut decoded_msgs = Vec::with_capacity(frames.len());
            let mut decode_errors = 0u64;
            for frame in frames {
                match self.codec.decode_frame(&frame) {
                    Ok(msg) => decoded_msgs.push(msg),
                    Err(_) => decode_errors += 1,
                }
            }

            let now = Utc::now();
            let mut hello_update = None;
            {
                let mut inner = self.inner.lock().await;
                if inner.generation != gen || inner.is_stopped {
                    return None;
                }
                inner.metrics.bytes_received = inner.metrics.bytes_received.wrapping_add(n as u64);
                inner.metrics.decode_errors = inner.metrics.decode_errors.wrapping_add(decode_errors);
                if !decoded_msgs.is_empty() {
                    inner.reconnect_attempt = 0;
                    inner.metrics.frames_received = inner
                        .metrics
                        .frames_received
                        .wrapping_add(decoded_msgs.len() as u64);
                    inner.metrics.last_frame_at = Some(now);
                }
            }

            for msg in decoded_msgs {
                match &msg {
                    WireMessage::Hello(hello) => {
                        hello_update = Some(hello.clone());
                    }
                    WireMessage::InputResult(res) => {
                        let waiter = self
                            .pending_inputs
                            .lock()
                            .ok()
                            .and_then(|mut g| g.remove(&res.id));
                        if let Some(tx) = waiter {
                            let _ = tx.send(Ok(res.clone()));
                        }
                    }
                    WireMessage::VisionFrame(frame) => {
                        let waiter = self
                            .pending_vision
                            .lock()
                            .ok()
                            .and_then(|mut g| g.remove(&frame.id));
                        if let Some(tx) = waiter {
                            let _ = tx.send(Ok(frame.clone()));
                        }
                    }
                    _ => {}
                }
                let _ = self.msg_tx.send(msg);
            }

            if let Some(hello) = hello_update {
                let _ = self
                    .set_state(
                        gen,
                        DaemonConnectionState::Connected { hello: Some(hello) },
                    )
                    .await;
            }
        }
    }

    async fn wait_before_reconnect(&self, gen: u64, last_error: Option<String>) -> bool {
        let (attempt, delay, hook) = {
            let mut inner = self.inner.lock().await;
            if inner.generation != gen || inner.is_stopped {
                return false;
            }
            inner.reconnect_attempt += 1;
            inner.metrics.reconnect_count = inner.metrics.reconnect_count.wrapping_add(1);
            let attempt = inner.reconnect_attempt;
            let exp_delay = self.config.initial_reconnect_delay_secs
                * 2.0_f64.powi((attempt.saturating_sub(1)).min(10) as i32);
            let capped = self.config.max_reconnect_delay_secs.min(exp_delay);
            let jitter: f64 = rand::rng().random_range(0.85..=1.15);
            let delay = (capped * jitter)
                .clamp(0.02, self.config.max_reconnect_delay_secs.max(0.02));
            (attempt, delay, inner.before_reconnect.clone())
        };

        if !self
            .set_state(
                gen,
                DaemonConnectionState::WaitingToReconnect {
                    attempt,
                    delay_seconds: delay,
                    last_error,
                },
            )
            .await
        {
            return false;
        }

        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        {
            let inner = self.inner.lock().await;
            if inner.generation != gen || inner.is_stopped {
                return false;
            }
        }

        if let Some(hook) = hook {
            hook(attempt).await;
        }

        let inner = self.inner.lock().await;
        inner.generation == gen && !inner.is_stopped
    }
}
