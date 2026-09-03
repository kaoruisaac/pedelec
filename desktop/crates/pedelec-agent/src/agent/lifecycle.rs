use super::error::AgentError;
#[cfg(test)]
use std::sync::Arc;
use std::sync::{Condvar, Mutex};

pub struct LifecycleGate {
    inner: Mutex<LifecycleInner>,
    commit_cvar: Condvar,
    #[cfg(test)]
    commit_admission_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct LifecycleInner {
    live: bool,
    generation: u64,
    commit_in_progress: bool,
}

pub struct CommitPermit<'a> {
    gate: &'a LifecycleGate,
}

impl LifecycleGate {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(LifecycleInner {
                live: true,
                generation: 1,
                commit_in_progress: false,
            }),
            commit_cvar: Condvar::new(),
            #[cfg(test)]
            commit_admission_hook: Mutex::new(None),
        }
    }

    pub fn generation(&self) -> u64 {
        lock_mutex(&self.inner).generation
    }

    pub fn is_open(&self) -> bool {
        lock_mutex(&self.inner).live
    }

    pub fn is_live(&self, generation: u64) -> bool {
        let inner = lock_mutex(&self.inner);
        inner.live && inner.generation == generation
    }

    pub fn emit_while_live(&self, generation: u64, write: impl FnOnce()) -> bool {
        let inner = lock_mutex(&self.inner);
        if !inner.live || inner.generation != generation {
            return false;
        }
        write();
        true
    }

    pub fn admit_inference_result(&self) -> Result<(), AgentError> {
        self.admit_live_work("The turn was invalidated before processing the inference result.")
    }

    pub fn admit_tool(&self) -> Result<(), AgentError> {
        self.admit_live_work("The turn was invalidated before starting the tool call.")
    }

    fn admit_live_work(&self, message: &str) -> Result<(), AgentError> {
        let inner = lock_mutex(&self.inner);
        if !inner.live {
            return Err(AgentError::new("TURN_INVALIDATED", message));
        }
        Ok(())
    }

    pub fn begin_commit(&self) -> Result<CommitPermit<'_>, AgentError> {
        #[cfg(test)]
        self.fire_commit_admission_hook();
        let mut inner = lock_mutex(&self.inner);
        if !inner.live {
            return Err(AgentError::new(
                "TURN_INVALIDATED",
                "The turn was invalidated before commit.",
            ));
        }
        inner.commit_in_progress = true;
        Ok(CommitPermit { gate: self })
    }

    fn end_commit(&self) {
        let mut inner = lock_mutex(&self.inner);
        inner.commit_in_progress = false;
        self.commit_cvar.notify_all();
    }

    pub fn close(&self) {
        let mut inner = lock_mutex(&self.inner);
        if inner.live {
            inner.live = false;
            inner.generation = inner.generation.saturating_add(1);
        }
        while inner.commit_in_progress {
            inner = self
                .commit_cvar
                .wait(inner)
                .unwrap_or_else(|err| err.into_inner());
        }
    }

    #[cfg(test)]
    pub fn set_commit_admission_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *lock_mutex(&self.commit_admission_hook) = hook;
    }

    #[cfg(test)]
    fn fire_commit_admission_hook(&self) {
        let hook = lock_mutex(&self.commit_admission_hook).clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl Drop for CommitPermit<'_> {
    fn drop(&mut self) {
        self.gate.end_commit();
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}
