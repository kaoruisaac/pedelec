use super::super::backend::ModelCapabilities;
use super::super::lifecycle::LifecycleGate;
use super::super::session::AgentSession;
use super::protocol::ProtocolWriter;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentState {
    Ready,
    Running { turn_id: String },
    Closed,
}

pub struct AttachmentControl {
    pub state: AttachmentState,
    pub committed_turn_ids: HashSet<String>,
}

pub struct SessionAttachment {
    pub thread_id: String,
    pub session_id: String,
    pub resumed: bool,
    pub capabilities: ModelCapabilities,
    pub model: String,
    pub workspace_path: PathBuf,
    pub host_instructions: Mutex<Option<String>>,
    pub generation: AtomicU64,
    pub live: Arc<AtomicBool>,
    pub gate: Arc<LifecycleGate>,
    pub control: Mutex<AttachmentControl>,
    pub session: Mutex<AgentSession>,
}

impl SessionAttachment {
    pub fn new(thread_id: String, session: AgentSession) -> Arc<Self> {
        let session_id = session.session_id().to_string();
        let resumed = session.resumed();
        let capabilities = session.capabilities();
        let model = session.model().to_string();
        let workspace_path = session.workspace_path().to_path_buf();
        let host_instructions = session.host_instructions().map(str::to_string);
        let committed_turn_ids = session.committed_turn_ids().clone();
        let live = session.live_token();
        let gate = session.lifecycle();
        let generation = gate.generation();
        Arc::new(Self {
            thread_id,
            session_id,
            resumed,
            capabilities,
            model,
            workspace_path,
            host_instructions: Mutex::new(host_instructions),
            generation: AtomicU64::new(generation),
            live,
            gate,
            control: Mutex::new(AttachmentControl {
                state: AttachmentState::Ready,
                committed_turn_ids,
            }),
            session: Mutex::new(session),
        })
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn emit(&self, writer: &ProtocolWriter, generation: u64, frame: Value) -> bool {
        self.emit_all(writer, generation, &[frame])
    }

    pub fn emit_all(&self, writer: &ProtocolWriter, generation: u64, frames: &[Value]) -> bool {
        self.gate.emit_while_live(generation, || {
            for frame in frames {
                writer.write_value(frame);
            }
        })
    }

    pub fn current_host_instructions(&self) -> Option<String> {
        lock_mutex(&self.host_instructions).clone()
    }

    pub fn set_cached_host_instructions(&self, host_instructions: Option<String>) {
        *lock_mutex(&self.host_instructions) = host_instructions;
    }

    pub fn close_lifecycle(&self) {
        self.live.store(false, Ordering::SeqCst);
        self.gate.close();
    }

    pub fn invalidate(&self) {
        self.live.store(false, Ordering::SeqCst);
        if let Ok(mut session) = self.session.try_lock() {
            session.close();
        }
        if let Ok(mut control) = self.control.try_lock() {
            control.state = AttachmentState::Closed;
        }
    }
}

enum InflightKind {
    Thread,
    Session,
}

pub struct InflightLock {
    inner: Arc<Mutex<RegistryInner>>,
    kind: InflightKind,
    key: String,
    lock: Arc<Mutex<()>>,
}

impl InflightLock {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        lock_mutex(&self.lock)
    }
}

impl Drop for InflightLock {
    fn drop(&mut self) {
        let mut inner = lock_mutex(&self.inner);
        let map = match self.kind {
            InflightKind::Thread => &mut inner.inflight_threads,
            InflightKind::Session => &mut inner.inflight_sessions,
        };
        if let Some(current) = map.get(&self.key) {
            if Arc::ptr_eq(current, &self.lock) && Arc::strong_count(current) == 2 {
                map.remove(&self.key);
            }
        }
    }
}

pub struct SessionRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    by_thread: HashMap<String, Arc<SessionAttachment>>,
    by_session: HashMap<String, Arc<SessionAttachment>>,
    inflight_threads: HashMap<String, Arc<Mutex<()>>>,
    inflight_sessions: HashMap<String, Arc<Mutex<()>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner::default())),
        }
    }

    pub fn lock_open(
        &self,
        thread_id: &str,
        session_id: Option<&str>,
    ) -> (InflightLock, Option<InflightLock>) {
        let mut inner = lock_mutex(&self.inner);
        let thread_lock = insert_inflight(
            &self.inner,
            &mut inner.inflight_threads,
            InflightKind::Thread,
            thread_id,
        );
        let session_lock = session_id.map(|session_id| {
            insert_inflight(
                &self.inner,
                &mut inner.inflight_sessions,
                InflightKind::Session,
                session_id,
            )
        });
        (thread_lock, session_lock)
    }

    pub fn get_by_thread(&self, thread_id: &str) -> Option<Arc<SessionAttachment>> {
        lock_mutex(&self.inner).by_thread.get(thread_id).cloned()
    }

    pub fn get_by_session(&self, session_id: &str) -> Option<Arc<SessionAttachment>> {
        lock_mutex(&self.inner).by_session.get(session_id).cloned()
    }

    pub fn lookup(
        &self,
        thread_id: &str,
        session_id: &str,
    ) -> Result<Arc<SessionAttachment>, RegistryLookup> {
        let inner = lock_mutex(&self.inner);
        match (
            inner.by_thread.get(thread_id).cloned(),
            inner.by_session.get(session_id).cloned(),
        ) {
            (Some(by_thread), _) if by_thread.session_id != session_id => {
                Err(RegistryLookup::Conflict {
                    thread_id: thread_id.to_string(),
                    attached_session_id: by_thread.session_id.clone(),
                    requested_session_id: session_id.to_string(),
                })
            }
            (_, Some(by_session)) if by_session.thread_id != thread_id => {
                Err(RegistryLookup::Conflict {
                    thread_id: by_session.thread_id.clone(),
                    attached_session_id: session_id.to_string(),
                    requested_session_id: session_id.to_string(),
                })
            }
            (Some(attachment), _) => Ok(attachment),
            (None, None) => Err(RegistryLookup::Missing),
            (None, Some(_)) => Err(RegistryLookup::Missing),
        }
    }

    pub fn register(
        &self,
        attachment: Arc<SessionAttachment>,
    ) -> Result<Arc<SessionAttachment>, RegisterError> {
        let mut inner = lock_mutex(&self.inner);
        if let Some(existing) = inner.by_thread.get(&attachment.thread_id) {
            if existing.session_id == attachment.session_id {
                return Ok(Arc::clone(existing));
            }
            return Err(RegisterError::ThreadBound {
                thread_id: attachment.thread_id.clone(),
                attached_session_id: existing.session_id.clone(),
            });
        }
        if let Some(existing) = inner.by_session.get(&attachment.session_id) {
            return Err(RegisterError::SessionBound {
                session_id: attachment.session_id.clone(),
                attached_thread_id: existing.thread_id.clone(),
            });
        }
        inner
            .by_thread
            .insert(attachment.thread_id.clone(), Arc::clone(&attachment));
        inner
            .by_session
            .insert(attachment.session_id.clone(), Arc::clone(&attachment));
        Ok(attachment)
    }

    pub fn detach(&self, attachment: &SessionAttachment) {
        let mut inner = lock_mutex(&self.inner);
        if inner
            .by_thread
            .get(&attachment.thread_id)
            .is_some_and(|current| current.session_id == attachment.session_id)
        {
            inner.by_thread.remove(&attachment.thread_id);
        }
        if inner
            .by_session
            .get(&attachment.session_id)
            .is_some_and(|current| current.thread_id == attachment.thread_id)
        {
            inner.by_session.remove(&attachment.session_id);
        }
    }

    pub fn resolve_close(
        &self,
        thread_id: &str,
        session_id: &str,
    ) -> Result<Option<Arc<SessionAttachment>>, RegistryLookup> {
        match self.lookup(thread_id, session_id) {
            Ok(attachment) => Ok(Some(attachment)),
            Err(RegistryLookup::Missing) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<SessionAttachment>> {
        lock_mutex(&self.inner)
            .by_thread
            .values()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn inflight_lock_counts(&self) -> (usize, usize) {
        let inner = lock_mutex(&self.inner);
        (inner.inflight_threads.len(), inner.inflight_sessions.len())
    }
}

fn insert_inflight(
    registry: &Arc<Mutex<RegistryInner>>,
    map: &mut HashMap<String, Arc<Mutex<()>>>,
    kind: InflightKind,
    key: &str,
) -> InflightLock {
    let lock = map
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    InflightLock {
        inner: Arc::clone(registry),
        kind,
        key: key.to_string(),
        lock,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryLookup {
    Missing,
    Conflict {
        thread_id: String,
        attached_session_id: String,
        requested_session_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    ThreadBound {
        thread_id: String,
        attached_session_id: String,
    },
    SessionBound {
        session_id: String,
        attached_thread_id: String,
    },
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn inflight_locks_are_removed_after_all_guards_drop() {
        let registry = SessionRegistry::new();
        {
            let (thread_lock, session_lock) = registry.lock_open("t1", Some("s1"));
            let _thread_guard = thread_lock.lock();
            let _session_guard = session_lock.as_ref().map(|lock| lock.lock());
            assert_eq!(registry.inflight_lock_counts(), (1, 1));
            let (again_thread, again_session) = registry.lock_open("t1", Some("s1"));
            assert!(Arc::ptr_eq(&thread_lock.lock, &again_thread.lock));
            assert!(Arc::ptr_eq(
                &session_lock.as_ref().unwrap().lock,
                &again_session.as_ref().unwrap().lock
            ));
            drop(again_thread);
            drop(again_session);
            assert_eq!(registry.inflight_lock_counts(), (1, 1));
        }
        assert_eq!(registry.inflight_lock_counts(), (0, 0));
    }

    #[test]
    fn inflight_lock_cleanup_does_not_split_mutual_exclusion() {
        let registry = SessionRegistry::new();
        let (first, _) = registry.lock_open("t1", None);
        let _guard = first.lock();
        let registry_for_waiter = SessionRegistry {
            inner: Arc::clone(&registry.inner),
        };
        let started = Arc::new(AtomicBool::new(false));
        let started_flag = Arc::clone(&started);
        let waiter = thread::spawn(move || {
            let (second, _) = registry_for_waiter.lock_open("t1", None);
            let _guard = second.lock();
            started_flag.store(true, Ordering::SeqCst);
        });
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(!started.load(Ordering::SeqCst));
        drop(_guard);
        drop(first);
        waiter.join().unwrap();
        assert_eq!(registry.inflight_lock_counts(), (0, 0));
    }
}
