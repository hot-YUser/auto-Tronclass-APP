//! The cancellation scope every monitor task lives in.

use crate::wire::{emit, EventCb, Problem};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};

/// One cancellation scope for a monitoring session. The actor, the pollers and every network
/// helper they spawn are registered here, so stopping the session is one `cancel`.
///
/// A task that panics is reported exactly once: a fixed `core_panicked` event for the UI and one
/// ping on `panic_tx` for the engine's watchdog. A cancelled task reports nothing.
///
/// The actor holds a clone of the group and the group holds the actor's task, so the group cannot
/// be dropped while the actor lives. Stopping a live session is therefore the owner's job
/// (`MonitorHandle::stop`, or its `Drop`); the group's own `Drop` only tidies up after that.
pub struct TaskGroup {
    cancelled: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    cb: EventCb,
    panic_tx: UnboundedSender<()>,
}

impl TaskGroup {
    pub fn new(cb: EventCb, panic_tx: UnboundedSender<()>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            cb,
            panic_tx,
        }
    }

    /// Run `future` in this scope. After `cancel` the task is aborted before it can run.
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let (cb, panic_tx) = (self.cb, self.panic_tx.clone());
        // The task runs inside its own JoinSet: dropping the wrapper aborts it, and a panic
        // surfaces as a JoinError here instead of vanishing with an unjoined handle.
        let wrapper = tokio::spawn(async move {
            let mut task = JoinSet::new();
            task.spawn(future);
            if matches!(task.join_next().await, Some(Err(error)) if error.is_panic()) {
                // The message is fixed: a panic payload may quote a secret.
                emit(
                    cb,
                    Problem::error("core_panicked", "monitor task failed internally"),
                );
                let _ = panic_tx.send(());
            }
        });
        // Registration and the cancelled check share the lock with `cancel`, so a spawn racing
        // a stop is either tracked and aborted by it, or aborted here.
        let mut tasks = self.tasks();
        tasks.retain(|task| !task.is_finished());
        if self.cancelled.load(Ordering::Acquire) {
            wrapper.abort();
        } else {
            tasks.push(wrapper);
        }
    }

    /// Abort every task and refuse new ones. Idempotent.
    pub fn cancel(&self) {
        let mut tasks = self.tasks();
        self.cancelled.store(true, Ordering::Release);
        for task in tasks.drain(..) {
            task.abort();
        }
    }

    /// The lock is never held across a panic, so a poisoned guard still holds a coherent list.
    fn tasks(&self) -> MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        // A dropped JoinHandle detaches its task; abort instead.
        for task in self.tasks().drain(..) {
            task.abort();
        }
    }
}

/// Cancels the group if `start` unwinds after spawning into it. Until the `MonitorHandle` exists
/// nothing else would stop those tasks.
pub(super) struct StartupGuard {
    group: Arc<TaskGroup>,
    armed: bool,
}

impl StartupGuard {
    pub(super) fn new(group: Arc<TaskGroup>) -> Self {
        Self { group, armed: true }
    }

    /// The handle now owns the stop guarantee.
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.group.cancel();
        }
    }
}
