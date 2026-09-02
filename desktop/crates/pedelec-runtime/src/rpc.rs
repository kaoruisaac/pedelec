use crate::jsonl::{JsonLineChannel, JsonLineError, JsonLineEvent};
use crate::persistent_process::PersistentWriter;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fmt;
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
    UnmatchedResponse { id: RpcId, response: Value },
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

struct RpcInner {
    writer: Mutex<Arc<dyn JsonLineWriter>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<RpcId, mpsc::SyncSender<Result<Value, RpcError>>>>,
    disconnected: Mutex<Option<RpcDisconnectReason>>,
    events: mpsc::Sender<RpcEvent>,
    envelope_mode: RpcEnvelopeMode,
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
        let inner = Arc::new(RpcInner {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            disconnected: Mutex::new(None),
            events: events_tx,
            envelope_mode,
        });
        let peer = Self {
            inner: Arc::clone(&inner),
            events: Arc::new(Mutex::new(events_rx)),
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
        let method = method.into();
        let id = RpcId::Number(
            i64::try_from(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
                .expect("RPC request id overflowed i64"),
        );
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let disconnected = self
                .inner
                .disconnected
                .lock()
                .expect("RPC disconnect mutex poisoned");
            if let Some(reason) = disconnected.clone() {
                return Err(RpcError::Disconnected { reason });
            }
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("RPC pending mutex poisoned");
            pending.insert(id.clone(), reply_tx);
        }

        let request =
            self.envelope(json!({ "id": id.as_value(), "method": method, "params": params }));
        let write_result = self
            .inner
            .writer
            .lock()
            .expect("RPC writer mutex poisoned")
            .write_json(&request);
        if let Err(error) = write_result {
            self.remove_pending(&id);
            self.mark_disconnected(RpcDisconnectReason::TransportIo(error.clone()));
            return Err(RpcError::Write { error });
        }

        match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.remove_pending(&id);
                Err(RpcError::RequestTimeout { id, method })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.remove_pending(&id);
                Err(RpcError::Disconnected {
                    reason: self.disconnect_reason(),
                })
            }
        }
    }

    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), RpcError> {
        let request = self.envelope(json!({ "method": method.into(), "params": params }));
        self.write_or_disconnect(&request)
    }

    pub fn respond_success(&self, id: RpcId, result: Value) -> Result<(), RpcError> {
        self.write_or_disconnect(&self.envelope(json!({ "id": id.as_value(), "result": result })))
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
        self.write_or_disconnect(&self.envelope(json!({ "id": id.as_value(), "error": error })))
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

    fn remove_pending(&self, id: &RpcId) {
        self.inner
            .pending
            .lock()
            .expect("RPC pending mutex poisoned")
            .remove(id);
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
    let waiter = inner
        .pending
        .lock()
        .expect("RPC pending mutex poisoned")
        .remove(&id);
    if let Some(waiter) = waiter {
        let _ = waiter.send(response);
    } else {
        let response = match response {
            Ok(response) => response,
            Err(error) => json!({ "error": error.to_string() }),
        };
        let _ = inner
            .events
            .send(RpcEvent::UnmatchedResponse { id, response });
    }

    Ok(())
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
        std::mem::take(&mut *inner.pending.lock().expect("RPC pending mutex poisoned"))
    };
    let error = RpcError::Disconnected {
        reason: reason.clone(),
    };
    for (_, waiter) in pending {
        let _ = waiter.send(Err(error.clone()));
    }
    let _ = inner.events.send(RpcEvent::Disconnected { reason });
}
