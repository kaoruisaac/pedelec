use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub trait ProviderRuntimeController: Send + Sync + fmt::Debug {
    fn shutdown(&self) -> Result<(), String>;

    /// A controller that lost its transport must be replaced by the next
    /// operation. Healthy controllers keep the default implementation so
    /// existing runtime implementations remain deliberately small.
    fn is_healthy(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLifecycle {
    Stopped,
    Starting,
    Ready,
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderRuntimeKey(String);

impl ProviderRuntimeKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ProviderRuntimeKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderRuntimeKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRegistryError {
    Initialization(String),
    ShuttingDown,
}

impl fmt::Display for RuntimeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialization(error) => {
                write!(f, "provider runtime initialization failed: {error}")
            }
            Self::ShuttingDown => write!(f, "provider runtime owner is shutting down"),
        }
    }
}

impl std::error::Error for RuntimeRegistryError {}

#[derive(Debug)]
enum EntryState {
    Vacant,
    Starting,
    Ready(Arc<dyn ProviderRuntimeController>),
    Stopping,
}

#[derive(Debug)]
struct RuntimeEntry {
    state: Mutex<EntryState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct RegistryState {
    entries: HashMap<ProviderRuntimeKey, Arc<RuntimeEntry>>,
}

/// Provider-scoped lazy runtime registry. Each key has at most one controller
/// and concurrent callers share one factory invocation.
#[derive(Debug, Default, Clone)]
pub struct ProviderRuntimeRegistry {
    state: Arc<Mutex<RegistryState>>,
    closed: Arc<AtomicBool>,
}

impl ProviderRuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_init<F>(
        &self,
        key: impl Into<ProviderRuntimeKey>,
        factory: F,
    ) -> Result<Arc<dyn ProviderRuntimeController>, RuntimeRegistryError>
    where
        F: FnOnce() -> Result<Arc<dyn ProviderRuntimeController>, RuntimeRegistryError>,
    {
        if self.closed.load(Ordering::Acquire) {
            return Err(RuntimeRegistryError::ShuttingDown);
        }
        let key = key.into();
        let entry = {
            let mut state = self.state.lock().expect("runtime registry mutex poisoned");
            Arc::clone(state.entries.entry(key).or_insert_with(|| {
                Arc::new(RuntimeEntry {
                    state: Mutex::new(EntryState::Vacant),
                    changed: Condvar::new(),
                })
            }))
        };

        let mut factory = Some(factory);
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(RuntimeRegistryError::ShuttingDown);
            }
            let mut entry_state = entry.state.lock().expect("runtime entry mutex poisoned");
            match &*entry_state {
                EntryState::Ready(controller) if controller.is_healthy() => {
                    return Ok(Arc::clone(controller));
                }
                EntryState::Ready(_) => {
                    // The provider runtime has retired its transport. Drop
                    // the registry's reference and allow this caller to
                    // become the single startup owner for the next
                    // generation.
                    *entry_state = EntryState::Vacant;
                    entry.changed.notify_all();
                }
                EntryState::Starting => {
                    entry_state = entry
                        .changed
                        .wait(entry_state)
                        .expect("runtime entry mutex poisoned");
                    drop(entry_state);
                }
                EntryState::Stopping => {
                    entry_state = entry
                        .changed
                        .wait(entry_state)
                        .expect("runtime entry mutex poisoned");
                    drop(entry_state);
                }
                EntryState::Vacant => {
                    if self.closed.load(Ordering::Acquire) {
                        return Err(RuntimeRegistryError::ShuttingDown);
                    }
                    *entry_state = EntryState::Starting;
                    drop(entry_state);
                    let result = catch_unwind(AssertUnwindSafe(
                        factory
                            .take()
                            .expect("runtime registry factory was consumed"),
                    ))
                    .map_err(|_| {
                        RuntimeRegistryError::Initialization("runtime factory panicked".to_string())
                    })
                    .and_then(|result| result);
                    let mut entry_state = entry.state.lock().expect("runtime entry mutex poisoned");
                    match if self.closed.load(Ordering::Acquire) {
                        if let Ok(controller) = &result {
                            let _ = controller.shutdown();
                        }
                        Err(RuntimeRegistryError::ShuttingDown)
                    } else {
                        result
                    } {
                        Ok(controller) => {
                            *entry_state = EntryState::Ready(Arc::clone(&controller));
                            entry.changed.notify_all();
                            return Ok(controller);
                        }
                        Err(error) => {
                            *entry_state = EntryState::Vacant;
                            entry.changed.notify_all();
                            return Err(error);
                        }
                    }
                }
            }
        }
    }

    pub fn get(
        &self,
        key: impl Into<ProviderRuntimeKey>,
    ) -> Option<Arc<dyn ProviderRuntimeController>> {
        let key = key.into();
        let entry = self
            .state
            .lock()
            .expect("runtime registry mutex poisoned")
            .entries
            .get(&key)
            .cloned()?;
        let result = match &*entry.state.lock().expect("runtime entry mutex poisoned") {
            EntryState::Ready(controller) => Some(Arc::clone(controller)),
            EntryState::Vacant | EntryState::Starting | EntryState::Stopping => None,
        };
        result
    }

    pub fn lifecycle(&self, key: impl Into<ProviderRuntimeKey>) -> Option<RuntimeLifecycle> {
        let key = key.into();
        let entry = self
            .state
            .lock()
            .expect("runtime registry mutex poisoned")
            .entries
            .get(&key)
            .cloned()?;
        let lifecycle = match &*entry.state.lock().expect("runtime entry mutex poisoned") {
            EntryState::Vacant => RuntimeLifecycle::Stopped,
            EntryState::Starting => RuntimeLifecycle::Starting,
            EntryState::Ready(_) => RuntimeLifecycle::Ready,
            EntryState::Stopping => RuntimeLifecycle::Stopping,
        };
        Some(lifecycle)
    }

    pub fn shutdown_all(&self) -> Vec<String> {
        self.closed.store(true, Ordering::Release);
        let entries = self
            .state
            .lock()
            .expect("runtime registry mutex poisoned")
            .entries
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for entry in entries {
            let controller = match &*entry.state.lock().expect("runtime entry mutex poisoned") {
                EntryState::Ready(controller) => Some(Arc::clone(controller)),
                EntryState::Vacant | EntryState::Starting | EntryState::Stopping => None,
            };
            if let Some(controller) = controller {
                {
                    let mut state = entry.state.lock().expect("runtime entry mutex poisoned");
                    *state = EntryState::Stopping;
                }
                if let Err(error) = controller.shutdown() {
                    errors.push(error);
                }
                let mut state = entry.state.lock().expect("runtime entry mutex poisoned");
                *state = EntryState::Vacant;
                entry.changed.notify_all();
            }
        }
        errors
    }
}

/// Desktop-level owner, intentionally separate from `CoreRuntimeOwner`.
#[derive(Debug, Clone)]
pub struct ProviderRuntimeOwner {
    registry: ProviderRuntimeRegistry,
    shutting_down: Arc<AtomicBool>,
    queued_operations: Arc<Mutex<Vec<(ProviderRuntimeKey, Value)>>>,
}

impl Default for ProviderRuntimeOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRuntimeOwner {
    pub fn new() -> Self {
        Self {
            registry: ProviderRuntimeRegistry::new(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            queued_operations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn registry(&self) -> ProviderRuntimeRegistry {
        self.registry.clone()
    }

    pub fn get_or_init<F>(
        &self,
        key: impl Into<ProviderRuntimeKey>,
        factory: F,
    ) -> Result<Arc<dyn ProviderRuntimeController>, RuntimeRegistryError>
    where
        F: FnOnce() -> Result<Arc<dyn ProviderRuntimeController>, RuntimeRegistryError>,
    {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(RuntimeRegistryError::ShuttingDown);
        }
        self.registry.get_or_init(key, factory)
    }

    /// Enqueues a semantic operation for the provider runtime layer. The
    /// operation is intentionally opaque here so Core and the reusable
    /// process/RPC transport do not become coupled to a provider protocol.
    pub fn enqueue_operation(
        &self,
        key: impl Into<ProviderRuntimeKey>,
        operation: Value,
    ) -> Result<(), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("provider runtime owner is shutting down".to_string());
        }
        self.queued_operations
            .lock()
            .map_err(|_| "provider runtime operation queue is poisoned".to_string())?
            .push((key.into(), operation));
        Ok(())
    }

    /// Test/bridge hook for a later provider-specific worker to consume
    /// operations accepted by the owner.
    pub fn take_queued_operations(&self) -> Vec<(ProviderRuntimeKey, Value)> {
        self.queued_operations
            .lock()
            .map(|mut operations| std::mem::take(&mut *operations))
            .unwrap_or_default()
    }

    pub fn shutdown(&self) -> Vec<String> {
        self.shutting_down.store(true, Ordering::Release);
        if let Ok(mut operations) = self.queued_operations.lock() {
            operations.clear();
        }
        self.registry.shutdown_all()
    }
}
