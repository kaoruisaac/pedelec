use crate::jsonl::{JsonLineChannel, JsonLineError};
use crate::owner::ProviderRuntimeController;
use crate::persistent_process::{
    PersistentProcess, PersistentProcessError, PersistentProcessSpec, ProcessExit,
};
use crate::rpc::{RpcEnvelopeMode, RpcError, RpcEvent, RpcId, RpcPeer};
use serde_json::Value;
use std::fmt;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum RuntimeCommand {
    Request {
        method: String,
        params: Value,
        timeout: Duration,
        reply: Sender<Result<Value, RpcError>>,
    },
    RequestScoped {
        owner: String,
        method: String,
        params: Value,
        timeout: Duration,
        reply: Sender<Result<Value, RpcError>>,
    },
    Notification {
        method: String,
        params: Value,
        reply: Sender<Result<(), RpcError>>,
    },
    RespondSuccess {
        id: RpcId,
        result: Value,
        reply: Sender<Result<(), RpcError>>,
    },
    RespondError {
        id: RpcId,
        code: Value,
        message: String,
        data: Option<Value>,
        reply: Sender<Result<(), RpcError>>,
    },
    Shutdown {
        grace_period: Duration,
        reply: Sender<Result<ProcessExit, RuntimeControllerError>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    Rpc(RpcEvent),
    ProcessExit(ProcessExit),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeControllerError {
    Process(PersistentProcessError),
    JsonLine(JsonLineError),
    Rpc(RpcError),
    CommandChannelClosed,
    WorkerChannelClosed,
    Shutdown(String),
}

impl fmt::Display for RuntimeControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(error) => write!(f, "{error}"),
            Self::JsonLine(error) => write!(f, "{error}"),
            Self::Rpc(error) => write!(f, "{error}"),
            Self::CommandChannelClosed => write!(f, "runtime worker command channel is closed"),
            Self::WorkerChannelClosed => write!(f, "runtime worker response channel is closed"),
            Self::Shutdown(error) => write!(f, "runtime shutdown failed: {error}"),
        }
    }
}

impl std::error::Error for RuntimeControllerError {}

/// Controller facade for a persistent process. Public calls enqueue commands;
/// the worker performs RPC waits and process shutdown outside any Core mutex.
pub struct PersistentRuntimeController {
    commands: SyncSender<RuntimeCommand>,
    events: Arc<Mutex<Receiver<RuntimeEvent>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    event_worker: Mutex<Option<thread::JoinHandle<()>>>,
    process: Arc<PersistentProcess>,
    peer: RpcPeer,
}

impl fmt::Debug for PersistentRuntimeController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentRuntimeController")
            .field("generation", &self.process.generation())
            .field("pid", &self.process.pid())
            .finish_non_exhaustive()
    }
}

impl PersistentRuntimeController {
    pub fn spawn(
        spec: PersistentProcessSpec,
        max_frame_bytes: usize,
    ) -> Result<Self, RuntimeControllerError> {
        Self::spawn_with_envelope_mode(spec, max_frame_bytes, RpcEnvelopeMode::Bare)
    }

    pub fn spawn_with_envelope_mode(
        spec: PersistentProcessSpec,
        max_frame_bytes: usize,
        envelope_mode: RpcEnvelopeMode,
    ) -> Result<Self, RuntimeControllerError> {
        let process =
            Arc::new(PersistentProcess::spawn(spec).map_err(RuntimeControllerError::Process)?);
        let stdout = process
            .take_stdout()
            .map_err(RuntimeControllerError::Process)?;
        let stderr = process
            .take_stderr()
            .map_err(RuntimeControllerError::Process)?;
        let channel = JsonLineChannel::spawn(stdout, stderr, max_frame_bytes)
            .map_err(RuntimeControllerError::JsonLine)?;
        let peer = RpcPeer::new_with_mode(channel, process.writer(), envelope_mode);
        let (commands_tx, commands_rx) = mpsc::sync_channel(64);
        let (events_tx, events_rx) = mpsc::channel();
        let exit_rx = process.subscribe_exit();
        let event_peer = peer.clone();
        let event_worker = thread::Builder::new()
            .name(format!("pedelec-runtime-events-{}", process.generation()))
            .spawn(move || forward_events(event_peer, exit_rx, events_tx))
            .expect("could not start runtime event worker");
        let command_peer = peer.clone();
        let command_worker = thread::Builder::new()
            .name(format!("pedelec-runtime-worker-{}", process.generation()))
            .spawn({
                let process = Arc::clone(&process);
                move || command_loop(command_peer, process, commands_rx)
            })
            .expect("could not start runtime command worker");

        Ok(Self {
            commands: commands_tx,
            events: Arc::new(Mutex::new(events_rx)),
            worker: Mutex::new(Some(command_worker)),
            event_worker: Mutex::new(Some(event_worker)),
            process,
            peer,
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process.pid()
    }

    pub fn generation(&self) -> u64 {
        self.process.generation()
    }

    pub fn is_healthy(&self) -> bool {
        self.process.is_running()
    }

    /// Retire a transport without waiting for a provider response. This is
    /// used when protocol correlation is no longer trustworthy.
    pub fn retire(&self) {
        let _ = self.process.retire();
    }

    pub fn request(
        &self,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RuntimeControllerError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(RuntimeCommand::Request {
                method: method.into(),
                params,
                timeout,
                reply: reply_tx,
            })
            .map_err(|_| RuntimeControllerError::CommandChannelClosed)?;
        reply_rx
            .recv()
            .map_err(|_| RuntimeControllerError::WorkerChannelClosed)?
            .map_err(RuntimeControllerError::Rpc)
    }

    pub fn request_scoped(
        &self,
        owner: impl Into<String>,
        method: impl Into<String>,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RuntimeControllerError> {
        let owner = owner.into();
        let method = method.into();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(RuntimeCommand::RequestScoped {
                owner,
                method,
                params,
                timeout,
                reply: reply_tx,
            })
            .map_err(|_| RuntimeControllerError::CommandChannelClosed)?;
        reply_rx
            .recv()
            .map_err(|_| RuntimeControllerError::WorkerChannelClosed)?
            .map_err(RuntimeControllerError::Rpc)
    }
    pub fn register_protocol_log(&self, owner: &str, provider: &str, workspace: &Path) {
        self.peer.register_protocol_log(owner, provider, workspace);
    }

    pub fn set_protocol_owner_resolver(
        &self,
        resolver: Arc<dyn Fn(&Value) -> Option<String> + Send + Sync>,
    ) {
        self.peer.set_protocol_owner_resolver(resolver);
    }

    pub fn notify(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<(), RuntimeControllerError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(RuntimeCommand::Notification {
                method: method.into(),
                params,
                reply: reply_tx,
            })
            .map_err(|_| RuntimeControllerError::CommandChannelClosed)?;
        reply_rx
            .recv()
            .map_err(|_| RuntimeControllerError::WorkerChannelClosed)?
            .map_err(RuntimeControllerError::Rpc)
    }

    pub fn respond_success(&self, id: RpcId, result: Value) -> Result<(), RuntimeControllerError> {
        self.send_response(|reply| RuntimeCommand::RespondSuccess { id, result, reply })
    }

    pub fn respond_error(
        &self,
        id: RpcId,
        code: Value,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Result<(), RuntimeControllerError> {
        self.send_response(|reply| RuntimeCommand::RespondError {
            id,
            code,
            message: message.into(),
            data,
            reply,
        })
    }

    pub fn recv_event_timeout(&self, timeout: Duration) -> Result<RuntimeEvent, RecvTimeoutError> {
        self.events
            .lock()
            .expect("runtime events mutex poisoned")
            .recv_timeout(timeout)
    }

    pub fn recv_event(&self) -> Result<RuntimeEvent, mpsc::RecvError> {
        self.events
            .lock()
            .expect("runtime events mutex poisoned")
            .recv()
    }

    pub fn shutdown_with_grace(
        &self,
        grace_period: Duration,
    ) -> Result<ProcessExit, RuntimeControllerError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(RuntimeCommand::Shutdown {
                grace_period,
                reply: reply_tx,
            })
            .map_err(|_| RuntimeControllerError::CommandChannelClosed)?;
        let result = reply_rx
            .recv()
            .map_err(|_| RuntimeControllerError::WorkerChannelClosed)??;
        if let Some(worker) = self
            .worker
            .lock()
            .expect("runtime worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        if let Some(worker) = self
            .event_worker
            .lock()
            .expect("runtime event worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        Ok(result)
    }

    fn send_response<F>(&self, command: F) -> Result<(), RuntimeControllerError>
    where
        F: FnOnce(Sender<Result<(), RpcError>>) -> RuntimeCommand,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(command(reply_tx))
            .map_err(|_| RuntimeControllerError::CommandChannelClosed)?;
        reply_rx
            .recv()
            .map_err(|_| RuntimeControllerError::WorkerChannelClosed)?
            .map_err(RuntimeControllerError::Rpc)
    }
}

impl ProviderRuntimeController for PersistentRuntimeController {
    fn shutdown(&self) -> Result<(), String> {
        self.shutdown_with_grace(Duration::from_secs(1))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

impl Drop for PersistentRuntimeController {
    fn drop(&mut self) {
        // Dropping a controller must not orphan a child that may be blocked
        // in a long RPC wait. The process classifies this as an expected
        // shutdown and the reader/waiter threads then unwind naturally.
        let _ = self.process.force_kill();
    }
}

fn command_loop(
    peer: RpcPeer,
    process: Arc<PersistentProcess>,
    commands: Receiver<RuntimeCommand>,
) {
    while let Ok(command) = commands.recv() {
        match command {
            RuntimeCommand::Request {
                method,
                params,
                timeout,
                reply,
            } => {
                let request_peer = peer.clone();
                let _ = thread::Builder::new()
                    .name("pedelec-runtime-rpc-command".to_string())
                    .spawn(move || {
                        let _ = reply.send(request_peer.request(method, params, timeout));
                    });
            }
            RuntimeCommand::RequestScoped {
                owner,
                method,
                params,
                timeout,
                reply,
            } => {
                let request_peer = peer.clone();
                let _ = thread::Builder::new()
                    .name("pedelec-runtime-rpc-command".to_string())
                    .spawn(move || {
                        let _ =
                            reply.send(request_peer.request_scoped(owner, method, params, timeout));
                    });
            }
            RuntimeCommand::Notification {
                method,
                params,
                reply,
            } => {
                let _ = reply.send(peer.notify(method, params));
            }
            RuntimeCommand::RespondSuccess { id, result, reply } => {
                let _ = reply.send(peer.respond_success(id, result));
            }
            RuntimeCommand::RespondError {
                id,
                code,
                message,
                data,
                reply,
            } => {
                let _ = reply.send(peer.respond_error(id, code, message, data));
            }
            RuntimeCommand::Shutdown {
                grace_period,
                reply,
            } => {
                peer.disconnect();
                let _ = reply.send(
                    process
                        .shutdown(grace_period)
                        .map_err(RuntimeControllerError::Process),
                );
                break;
            }
        }
    }
}

fn forward_events(peer: RpcPeer, exit_rx: Receiver<ProcessExit>, events: Sender<RuntimeEvent>) {
    let mut rpc_closed = false;
    let mut process_closed = false;
    while !(rpc_closed && process_closed) {
        if !rpc_closed {
            match peer.recv_event_timeout(Duration::from_millis(20)) {
                Ok(event) => {
                    if matches!(event, RpcEvent::Disconnected { .. }) {
                        rpc_closed = true;
                    }
                    if events.send(RuntimeEvent::Rpc(event)).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => rpc_closed = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if !process_closed {
            match exit_rx.try_recv() {
                Ok(exit) => {
                    process_closed = true;
                    if events.send(RuntimeEvent::ProcessExit(exit)).is_err() {
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => process_closed = true,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }
}
