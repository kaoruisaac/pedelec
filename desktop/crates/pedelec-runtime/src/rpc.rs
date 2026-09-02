use crate::jsonl::{JsonLineChannel, JsonLineError, JsonLineEvent};
use crate::persistent_process::PersistentWriter;
use crate::protocol::{ProtocolTrafficLogger, ProtocolTrafficRecord};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Controls the envelope emitted by [`RpcPeer`]. Codex App Server uses the
/// historical bare shape while ACP requires strict JSON-RPC 2.0 envelopes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RpcEnvelopeMode {
    #[default]
    Bare,
    JsonRpc2,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RpcId {
    Number(i64),
    String(String),
}

impl RpcId {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => number.as_i64().map(Self::Number).or_else(|| {
                number
                    .as_u64()
                    .and_then(|value| i64::try_from(value).ok())
                    .map(Self::Number)
            }),
            Value::String(value) => Some(Self::String(value.clone())),
            _ => None,
        }
    }

    fn as_value(&self) -> Value {
        match self {
            Self::Number(value) => json!(value),
            Self::String(value) => json!(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcServerRequest {
    pub id: RpcId,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcDisconnectReason {
    CleanEof,
    UncleanEof,
    MalformedFrame(String),
    TransportIo(String),
    Explicit,
    ChannelClosed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcEvent {
    Notification { method: String, params: Value },
    ServerRequest(RpcServerRequest),
    Stderr { text: String },
    Disconnected { reason: RpcDisconnectReason },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    RequestTimeout {
        id: RpcId,
        method: String,
    },
    Disconnected {
        reason: RpcDisconnectReason,
    },
    Write {
        error: String,
    },
    Remote {
        code: Option<Value>,
        message: String,
        data: Option<Value>,
    },
    InvalidMessage {
        message: String,
    },
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestTimeout { id, method } => {
                write!(f, "RPC request {method} ({id:?}) timed out")
            }
            Self::Disconnected { reason } => write!(f, "RPC transport disconnected: {reason:?}"),
            Self::Write { error } => write!(f, "RPC write failed: {error}"),
            Self::Remote { message, .. } => write!(f, "remote RPC error: {message}"),
            Self::InvalidMessage { message } => write!(f, "invalid RPC message: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

type OwnerResolver = Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>;

struct PendingRequest {
    reply: mpsc::SyncSender<Result<Value, RpcError>>,
    owner: Option<String>,
}

const MAX_LATE_RESPONSE_OWNERS: usize = 1024;

#[derive(Default)]
struct RpcCorrelationState {
    pending: HashMap<RpcId, PendingRequest>,
    late_response_owners: HashMap<RpcId, String>,
    late_response_order: VecDeque<RpcId>,
}

impl RpcCorrelationState {
    fn remember_late_response_owner(&mut self, id: RpcId, owner: Option<String>) {
        let Some(owner) = owner else {
            return;
        };
        self.late_response_owners.insert(id.clone(), owner);
        self.late_response_order.push_back(id);
        while self.late_response_order.len() > MAX_LATE_RESPONSE_OWNERS {
            if let Some(expired) = self.late_response_order.pop_front() {
                self.late_response_owners.remove(&expired);
            }
        }
    }

    fn take_late_response_owner(&mut self, id: &RpcId) -> Option<String> {
        self.late_response_owners.remove(id)
    }
}

#[derive(Default)]
struct RpcProtocolState {
    resolver: Option<OwnerResolver>,
    server_request_owners: HashMap<RpcId, String>,
}

struct RpcInner {
    writer: Mutex<Arc<dyn JsonLineWriter>>,
    next_id: AtomicU64,
    correlations: Mutex<RpcCorrelationState>,
    disconnected: Mutex<Option<RpcDisconnectReason>>,
    events: mpsc::Sender<RpcEvent>,
    traffic: mpsc::Sender<ProtocolTrafficRecord>,
    envelope_mode: RpcEnvelopeMode,
    protocol_state: Mutex<RpcProtocolState>,
    protocol_logger: ProtocolTrafficLogger,
}

/// A writer abstraction keeps `RpcPeer` reusable with persistent process
/// stdin while allowing deterministic in-memory transport tests.
pub trait JsonLineWriter: Send + Sync {
    fn write_json(&self, value: &Value) -> Result<(), String>;
}

impl JsonLineWriter for PersistentWriter {
    fn write_json(&self, value: &Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        self.write_all(&bytes).map_err(|error| error.to_string())?;
        self.flush().map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
pub struct RpcPeer {
    inner: Arc<RpcInner>,
    events: Arc<Mutex<mpsc::Receiver<RpcEvent>>>,
    traffic: Arc<Mutex<mpsc::Receiver<ProtocolTrafficRecord>>>,
}

impl fmt::Debug for RpcPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcPeer").finish_non_exhaustive()
    }
}

impl RpcPeer {
    pub fn new(channel: JsonLineChannel, writer: PersistentWriter) -> Self {
        Self::new_with_mode(channel, writer, RpcEnvelopeMode::Bare)
    }

    pub fn new_with_mode(
        channel: JsonLineChannel,
        writer: PersistentWriter,
        envelope_mode: RpcEnvelopeMode,
    ) -> Self {
        Self::new_with_writer_and_mode(channel, Arc::new(writer), envelope_mode)
    }

    pub fn new_with_writer(channel: JsonLineChannel, writer: Arc<dyn JsonLineWriter>) -> Self {
        Self::new_with_writer_and_mode(channel, writer, RpcEnvelopeMode::Bare)
    }

    pub fn new_with_writer_and_mode(
        channel: JsonLineChannel,
        writer: Arc<dyn JsonLineWriter>,
        envelope_mode: RpcEnvelopeMode,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let (traffic_tx, traffic_rx) = mpsc::channel();
        let inner = Arc::new(RpcInner {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            correlations: Mutex::new(RpcCorrelationState::default()),
            disconnected: Mutex::new(None),
            events: events_tx,
            traffic: traffic_tx,
            envelope_mode,
            protocol_state: Mutex::new(RpcProtocolState::default()),
            protocol_logger: ProtocolTrafficLogger::default(),
        });
        let peer = Self {
            inner: Arc::clone(&inner),
            events: Arc::new(Mutex::new(events_rx)),
            traffic: Arc::new(Mutex::new(traffic_rx)),
        };
        spawn_reader_loop(inner, channel);
        peer
    }

    pub fn request(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.request_with_owner(None, method.into(), params, timeout)
    }

    pub fn request_scoped(
        &self,
        owner: impl Into<String>,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.request_with_owner(Some(owner.into()), method.into(), params, timeout)
    }

    fn request_with_owner(
        &self,
        owner: Option<String>,
        method: String,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = RpcId::Number(
            i64::try_from(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
                .expect("RPC request id overflowed i64"),
        );
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let request =
            self.envelope(json!({ "id": id.as_value(), "method": method, "params": params }));
        let effective_owner = owner.or_else(|| resolve_protocol_owner(&self.inner, &request));
        {
            let disconnected = self
                .inner
                .disconnected
                .lock()
                .expect("RPC disconnect mutex poisoned");
            if let Some(reason) = disconnected.clone() {
                return Err(RpcError::Disconnected { reason });
            }
            let mut correlations = self
                .inner
                .correlations
                .lock()
                .expect("RPC correlation mutex poisoned");
            correlations.pending.insert(
                id.clone(),
                PendingRequest {
                    reply: reply_tx,
                    owner: effective_owner.clone(),
                },
            );
        }

        let write_result = self
            .inner
            .writer
            .lock()
            .expect("RPC writer mutex poisoned")
            .write_json(&request);
        if let Err(error) = write_result {
            let _ = self.remove_pending(&id);
            self.mark_disconnected(RpcDisconnectReason::TransportIo(error.clone()));
            return Err(RpcError::Write { error });
        }
        self.capture_protocol_frame(
            "client_to_provider",
            "request",
            &request,
            effective_owner.as_deref(),
            false,
        );

        match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.timeout_pending(&id);
                Err(RpcError::RequestTimeout { id, method })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.remove_pending(&id);
                Err(RpcError::Disconnected {
                    reason: self.disconnect_reason(),
                })
            }
        }
    }

    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), RpcError> {
        let request = self.envelope(json!({ "method": method.into(), "params": params }));
        self.write_or_disconnect(&request)?;
        self.capture_protocol_frame("client_to_provider", "notification", &request, None, false);
        Ok(())
    }

    pub fn respond_success(&self, id: RpcId, result: Value) -> Result<(), RpcError> {
        let response = self.envelope(json!({ "id": id.as_value(), "result": result }));
        let owner = self.take_server_request_owner(&id);
        self.write_or_disconnect(&response)?;
        self.capture_protocol_frame(
            "client_to_provider",
            "response",
            &response,
            owner.as_deref(),
            false,
        );
        Ok(())
    }

    pub fn respond_error(
        &self,
        id: RpcId,
        code: Value,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Result<(), RpcError> {
        let mut error = Map::new();
        error.insert("code".to_string(), code);
        error.insert("message".to_string(), Value::String(message.into()));
        if let Some(data) = data {
            error.insert("data".to_string(), data);
        }
        let response = self.envelope(json!({ "id": id.as_value(), "error": error }));
        let owner = self.take_server_request_owner(&id);
        self.write_or_disconnect(&response)?;
        self.capture_protocol_frame(
            "client_to_provider",
            "response",
            &response,
            owner.as_deref(),
            false,
        );
        Ok(())
    }

    pub fn register_protocol_log(&self, owner: &str, provider: &str, workspace: &Path) {
        self.inner
            .protocol_logger
            .register_protocol_log(owner, provider, workspace);
    }

    pub fn set_protocol_owner_resolver(
        &self,
        resolver: Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>,
    ) {
        self.inner
            .protocol_state
            .lock()
            .expect("RPC protocol state mutex poisoned")
            .resolver = Some(resolver);
    }

    pub fn recv_event(&self) -> Result<RpcEvent, mpsc::RecvError> {
        self.events
            .lock()
            .expect("RPC events mutex poisoned")
            .recv()
    }

    pub fn recv_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<RpcEvent, mpsc::RecvTimeoutError> {
        self.events
            .lock()
            .expect("RPC events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn try_recv_traffic(&self) -> Result<ProtocolTrafficRecord, mpsc::TryRecvError> {
        self.traffic
            .lock()
            .expect("RPC traffic mutex poisoned")
            .try_recv()
    }

    pub fn recv_traffic_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProtocolTrafficRecord, mpsc::RecvTimeoutError> {
        self.traffic
            .lock()
            .expect("RPC traffic mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn disconnect(&self) {
        self.mark_disconnected(RpcDisconnectReason::Explicit);
    }

    pub fn is_connected(&self) -> bool {
        self.inner
            .disconnected
            .lock()
            .expect("RPC disconnect mutex poisoned")
            .is_none()
    }

    fn write_or_disconnect(&self, value: &Value) -> Result<(), RpcError> {
        if let Some(reason) = self
            .inner
            .disconnected
            .lock()
            .expect("RPC disconnect mutex poisoned")
            .clone()
        {
            return Err(RpcError::Disconnected { reason });
        }
        let write_result = self
            .inner
            .writer
            .lock()
            .expect("RPC writer mutex poisoned")
            .write_json(value);
        write_result.map_err(|error| {
            self.mark_disconnected(RpcDisconnectReason::TransportIo(error.clone()));
            RpcError::Write { error }
        })
    }

    fn envelope(&self, mut value: Value) -> Value {
        if self.inner.envelope_mode == RpcEnvelopeMode::JsonRpc2 {
            value
                .as_object_mut()
                .expect("outbound RPC envelopes are objects")
                .insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        }
        value
    }

    fn capture_protocol_frame(
        &self,
        direction: &str,
        kind: &str,
        message: &Value,
        explicit_owner: Option<&str>,
        unmatched: bool,
    ) {
        capture_protocol_frame(
            &self.inner,
            direction,
            kind,
            message,
            explicit_owner,
            unmatched,
        );
    }

    fn take_server_request_owner(&self, id: &RpcId) -> Option<String> {
        self.inner
            .protocol_state
            .lock()
            .expect("RPC protocol state mutex poisoned")
            .server_request_owners
            .remove(id)
    }

    fn remove_pending(&self, id: &RpcId) -> Option<PendingRequest> {
        self.inner
            .correlations
            .lock()
            .expect("RPC correlation mutex poisoned")
            .pending
            .remove(id)
    }

    fn timeout_pending(&self, id: &RpcId) {
        let mut correlations = self
            .inner
            .correlations
            .lock()
            .expect("RPC correlation mutex poisoned");
        let owner = correlations
            .pending
            .remove(id)
            .and_then(|pending| pending.owner);
        correlations.remember_late_response_owner(id.clone(), owner);
    }

    fn disconnect_reason(&self) -> RpcDisconnectReason {
        self.inner
            .disconnected
            .lock()
            .expect("RPC disconnect mutex poisoned")
            .clone()
            .unwrap_or(RpcDisconnectReason::ChannelClosed)
    }

    fn mark_disconnected(&self, reason: RpcDisconnectReason) {
        mark_disconnected(&self.inner, reason);
    }
}

fn spawn_reader_loop(inner: Arc<RpcInner>, channel: JsonLineChannel) {
    thread::Builder::new()
        .name("pedelec-runtime-rpc-reader".to_string())
        .spawn(move || loop {
            let event = match channel.recv() {
                Ok(event) => event,
                Err(_) => {
                    mark_disconnected(&inner, RpcDisconnectReason::ChannelClosed);
                    break;
                }
            };
            match event {
                JsonLineEvent::StdoutFrame(frame) => match dispatch_frame(&inner, frame) {
                    Ok(()) => {}
                    Err(error) => {
                        let reason = match error {
                            RpcError::InvalidMessage { message } => {
                                RpcDisconnectReason::MalformedFrame(message)
                            }
                            other => RpcDisconnectReason::MalformedFrame(other.to_string()),
                        };
                        mark_disconnected(&inner, reason);
                        break;
                    }
                },
                JsonLineEvent::StderrChunk(text) => {
                    let _ = inner.events.send(RpcEvent::Stderr { text });
                }
                JsonLineEvent::StdoutEof { clean } => {
                    mark_disconnected(
                        &inner,
                        if clean {
                            RpcDisconnectReason::CleanEof
                        } else {
                            RpcDisconnectReason::UncleanEof
                        },
                    );
                    break;
                }
                JsonLineEvent::StderrEof => {}
                JsonLineEvent::Error(error) => {
                    let reason = match &error {
                        JsonLineError::Io { error, .. } => {
                            RpcDisconnectReason::TransportIo(error.clone())
                        }
                        JsonLineError::UncleanEof { .. } => RpcDisconnectReason::UncleanEof,
                        _ => RpcDisconnectReason::MalformedFrame(error.to_string()),
                    };
                    mark_disconnected(&inner, reason);
                    break;
                }
            }
        })
        .expect("could not start RPC reader loop");
}

fn dispatch_frame(inner: &Arc<RpcInner>, frame: Value) -> Result<(), RpcError> {
    let object = frame.as_object().ok_or_else(|| RpcError::InvalidMessage {
        message: "RPC frame must be a JSON object".to_string(),
    })?;
    let has_method = object.contains_key("method");
    let method = object.get("method").and_then(Value::as_str);
    let has_id = object.contains_key("id");
    let id = object.get("id").and_then(RpcId::from_value);

    if has_method {
        let method = method
            .filter(|method| !method.trim().is_empty())
            .ok_or_else(|| RpcError::InvalidMessage {
                message: "RPC method must be a non-empty string".to_string(),
            })?;
        if object.contains_key("result") || object.contains_key("error") {
            return Err(RpcError::InvalidMessage {
                message: "RPC request/notification cannot contain result or error".to_string(),
            });
        }
        if has_id && id.is_none() {
            return Err(RpcError::InvalidMessage {
                message: "RPC request id must be a string or integer".to_string(),
            });
        }
        match id {
            Some(id) => {
                let owner = resolve_protocol_owner(inner, &frame);
                capture_protocol_frame(
                    inner,
                    "provider_to_client",
                    "request",
                    &frame,
                    owner.as_deref(),
                    false,
                );
                if let Some(owner) = owner {
                    inner
                        .protocol_state
                        .lock()
                        .expect("RPC protocol state mutex poisoned")
                        .server_request_owners
                        .insert(id.clone(), owner);
                }
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                inner
                    .events
                    .send(RpcEvent::ServerRequest(RpcServerRequest {
                        id,
                        method: method.to_string(),
                        params,
                    }))
                    .map_err(|_| RpcError::Disconnected {
                        reason: RpcDisconnectReason::ChannelClosed,
                    })?;
            }
            None => {
                capture_protocol_frame(
                    inner,
                    "provider_to_client",
                    "notification",
                    &frame,
                    None,
                    false,
                );
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                inner
                    .events
                    .send(RpcEvent::Notification {
                        method: method.to_string(),
                        params,
                    })
                    .map_err(|_| RpcError::Disconnected {
                        reason: RpcDisconnectReason::ChannelClosed,
                    })?;
            }
        }
        return Ok(());
    }

    if !has_id || id.is_none() {
        return Err(RpcError::InvalidMessage {
            message: "RPC frame must contain method or a valid response id".to_string(),
        });
    }
    if object.contains_key("result") == object.contains_key("error") {
        return Err(RpcError::InvalidMessage {
            message: "RPC response must contain exactly one of result or error".to_string(),
        });
    }
    let id = id.expect("validated RPC response id");
    let response = if let Some(error) = object.get("error") {
        Err(parse_remote_error(error)?)
    } else {
        Ok(object.get("result").cloned().expect("validated RPC result"))
    };
    let (pending, late_owner) = {
        let mut correlations = inner
            .correlations
            .lock()
            .expect("RPC correlation mutex poisoned");
        let pending = correlations.pending.remove(&id);
        let late_owner = if pending.is_none() {
            correlations.take_late_response_owner(&id)
        } else {
            None
        };
        (pending, late_owner)
    };
    let owner = pending
        .as_ref()
        .and_then(|pending| pending.owner.clone())
        .or(late_owner);
    let unmatched = pending.is_none();
    capture_protocol_frame(
        inner,
        "provider_to_client",
        "response",
        &frame,
        owner.as_deref(),
        unmatched,
    );
    if let Some(pending) = pending {
        let _ = pending.reply.send(response);
    }

    Ok(())
}

fn resolve_protocol_owner(inner: &Arc<RpcInner>, frame: &Value) -> Option<String> {
    let resolver = inner
        .protocol_state
        .lock()
        .expect("RPC protocol state mutex poisoned")
        .resolver
        .clone();
    resolver.and_then(|resolver| resolver(frame))
}

fn capture_protocol_frame(
    inner: &Arc<RpcInner>,
    direction: &str,
    kind: &str,
    message: &Value,
    explicit_owner: Option<&str>,
    unmatched: bool,
) {
    let owner = explicit_owner
        .map(str::to_string)
        .or_else(|| resolve_protocol_owner(inner, message));
    let traffic =
        inner
            .protocol_logger
            .capture(direction, kind, message, owner.as_deref(), unmatched);
    let _ = inner.traffic.send(traffic);
}

fn parse_remote_error(error: &Value) -> Result<RpcError, RpcError> {
    let object = error.as_object().ok_or_else(|| RpcError::InvalidMessage {
        message: "RPC error must be a JSON object".to_string(),
    })?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| RpcError::InvalidMessage {
            message: "RPC error message must be a non-empty string".to_string(),
        })?;
    Ok(RpcError::Remote {
        code: object.get("code").cloned(),
        message: message.to_string(),
        data: object.get("data").cloned(),
    })
}

fn mark_disconnected(inner: &Arc<RpcInner>, reason: RpcDisconnectReason) {
    let pending = {
        let mut disconnected = inner
            .disconnected
            .lock()
            .expect("RPC disconnect mutex poisoned");
        if disconnected.is_some() {
            return;
        }
        *disconnected = Some(reason.clone());
        let mut correlations = inner
            .correlations
            .lock()
            .expect("RPC correlation mutex poisoned");
        correlations.late_response_owners.clear();
        correlations.late_response_order.clear();
        std::mem::take(&mut correlations.pending)
    };
    let error = RpcError::Disconnected {
        reason: reason.clone(),
    };
    for (_, pending) in pending {
        let _ = pending.reply.send(Err(error.clone()));
    }
    let _ = inner.events.send(RpcEvent::Disconnected { reason });
}
