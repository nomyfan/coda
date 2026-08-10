//! The two pieces of pure session-scheduling state: the single-flight slot for
//! the active turn, and the ledger of calls still owed an answer. Both are
//! plain in-memory state machines; deciding *when* they change — and what else
//! changes with them (broadcasts, storage) — stays with `AgentRuntime`.

use crate::ThreadId;
use coda_core::llm::TurnId;
use std::collections::HashMap;
use std::sync::Mutex;

/// The turn this session has been asked to run and has not finished.
///
/// Only a user `Task` opens a turn — a `ToolCall` carries its caller's turn id,
/// and `Reply`/`Resume` continue work that is already registered.
struct ActiveTurn {
    id: TurnId,
    cancelled: bool,
}

/// Rejected by [`TurnGate::open`]: the session already has an active turn.
#[derive(Debug)]
pub(super) struct TurnAlreadyActive;

/// The single-flight slot for the session's active turn. All transitions go
/// through these methods; the slot itself is private so nothing can bypass the
/// single-flight check or the cancellation mark.
#[derive(Default)]
pub(super) struct TurnGate {
    /// A blocking mutex on purpose: every critical section below is a handful
    /// of small state updates and none of them awaits.
    slot: Mutex<Option<ActiveTurn>>,
}

impl TurnGate {
    pub(super) fn active_id(&self) -> Option<TurnId> {
        self.slot
            .lock()
            .expect("active turn")
            .as_ref()
            .map(|active| active.id)
    }

    /// Register the turn a user task opens, before anything can act on it.
    pub(super) fn open(&self, turn: TurnId) -> Result<(), TurnAlreadyActive> {
        let mut active = self.slot.lock().expect("active turn");
        if active.is_some() {
            return Err(TurnAlreadyActive);
        }
        *active = Some(ActiveTurn {
            id: turn,
            cancelled: false,
        });
        Ok(())
    }

    /// Restore one piece of evidence for an active turn. Several threads and
    /// envelopes may name the same turn; a different id violates single-flight.
    pub(super) fn restore(&self, turn: TurnId) -> Result<(), String> {
        let mut active = self.slot.lock().expect("active turn");
        match active.as_ref() {
            Some(current) if current.id == turn => Ok(()),
            Some(current) => Err(format!(
                "runtime snapshot contains active turns {} and {}",
                current.id, turn
            )),
            None => {
                *active = Some(ActiveTurn {
                    id: turn,
                    cancelled: false,
                });
                Ok(())
            }
        }
    }

    /// Drop a finished turn. Idempotent — a turn that was never registered, or
    /// was already closed, is simply absent.
    pub(super) fn close(&self, turn: TurnId) {
        let mut active = self.slot.lock().expect("active turn");
        if active.as_ref().is_some_and(|current| current.id == turn) {
            *active = None;
        }
    }

    /// Mark a named turn as asked to stop, if it is the active one.
    pub(super) fn cancel(&self, turn: TurnId) {
        if let Some(active) = self.slot.lock().expect("active turn").as_mut()
            && active.id == turn
        {
            active.cancelled = true;
        }
    }

    /// Mark whatever turn is active as asked to stop.
    pub(super) fn cancel_active(&self) {
        if let Some(active) = self.slot.lock().expect("active turn").as_mut() {
            active.cancelled = true;
        }
    }

    /// Whether this turn has been asked to stop. Agents read the mark rather
    /// than deciding for themselves which turn an abort meant: a stateless
    /// agent's `Agent` instance is reused across threads, so while it sits idle
    /// its own `current_turn()` still names the previous one.
    pub(super) fn is_cancelled(&self, turn: TurnId) -> bool {
        self.slot
            .lock()
            .expect("active turn")
            .as_ref()
            .is_some_and(|active| active.id == turn && active.cancelled)
    }
}

/// Calls dispatched to a thread that has not been answered yet, counted per
/// thread.
///
/// The count spans the whole obligation — from the call going out to the
/// caller consuming the answer — rather than just the wait in the inbox. A
/// thread that already took its envelope and is now itself waiting on a
/// sub-agent of its own is still working, and treating it as gone is what
/// would make a caller write its result for it.
#[derive(Default)]
pub(super) struct CallLedger {
    unanswered: Mutex<HashMap<ThreadId, usize>>,
}

impl CallLedger {
    /// Note that a call has gone out to `thread_id` and has not been answered.
    pub(super) fn begin(&self, thread_id: &ThreadId) {
        *self
            .unanswered
            .lock()
            .expect("unanswered calls")
            .entry(thread_id.clone())
            .or_insert(0) += 1;
    }

    /// Note that one of `thread_id`'s callers has taken its answer.
    pub(super) fn end(&self, thread_id: &ThreadId) {
        let mut unanswered = self.unanswered.lock().expect("unanswered calls");
        if let Some(count) = unanswered.get_mut(thread_id) {
            *count -= 1;
            if *count == 0 {
                unanswered.remove(thread_id);
            }
        }
    }

    /// Whether this thread still owes somebody an answer in this process.
    ///
    /// `false` means nothing here will ever produce that answer — the work went
    /// away with a previous process — and the caller is free to write the call
    /// off rather than wait forever.
    pub(super) fn is_answering(&self, thread_id: &ThreadId) -> bool {
        self.unanswered
            .lock()
            .expect("unanswered calls")
            .contains_key(thread_id)
    }
}
