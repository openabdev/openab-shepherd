//! The task ledger: what was dispatched to which worker session, in which
//! state, and whether the worker reported it complete.
//!
//! The ledger is a pure component. It does no network, PTY or MCP work, reads
//! nothing from the rest of this crate and is not called by the server. It
//! depends only on `serde_json`, `sha2` and `libc`. Its public surface is the
//! `ledger` module, which is why no item here is dead code.
//!
//! What it holds:
//!
//! - an append-only event log, synced before any effect it describes, as the
//!   single source of truth ([`Store`], [`FileStore`], [`log`]);
//! - the dispatch states and transitions ([`State`], [`LedgerState`]);
//! - the matching rule for a worker record ([`match_record`]), the only way a
//!   dispatch reaches `done`;
//! - crash recovery: a dispatch found in `delivering` becomes
//!   `delivery_unknown`, never `ready` and never `done`.
//!
//! See `docs/ledger.md` for the on-disk format and what the design does and
//! does not guarantee.

mod event;
mod log;
mod state;
mod store;
mod types;

#[cfg(test)]
mod tests;

pub use event::Event;
pub use log::{encode_record, replay, Replay, ReplayError, FORMAT_VERSION};
pub use state::{match_record, Claim, LedgerState, TransitionError, MAX_RETRIES_LIMIT};
pub use store::{FileStore, Store};
pub use types::{
    BadContentRef, Code, ContentRef, Dispatch, DispatchId, NameError, Outcome, RejectReason,
    SessionId, State, MAX_NAME_LEN,
};

use std::fmt;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. Injected so tests need no sleeps.
pub type Clock = Box<dyn Fn() -> u64 + Send>;

pub fn system_clock() -> Clock {
    Box::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    })
}

/// A ledger operation failed.
#[derive(Debug)]
pub enum LedgerError {
    /// The operation does not fit the current state. Nothing was written,
    /// with one exception: reaching the retry cap in `begin_delivery` or
    /// `request_retry` logs `failed` first and then returns `RetryCapReached`.
    Transition(TransitionError),
    /// The log could not be written. Nothing changed in memory. The ledger
    /// refuses every further write until it is reopened; reads still work.
    Store(io::Error),
    /// An earlier write failed, so this one was refused without trying.
    LogUnavailable,
    /// The existing log could not be replayed.
    Replay(ReplayError),
    /// The log could not be read or cut at open.
    Open(io::Error),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::Transition(e) => write!(f, "{e}"),
            LedgerError::Store(e) => write!(f, "ledger log write failed: {e}"),
            LedgerError::LogUnavailable => {
                f.write_str("ledger log is unavailable after an earlier write failure")
            }
            LedgerError::Replay(e) => write!(f, "{e}"),
            LedgerError::Open(e) => write!(f, "ledger log could not be opened: {e}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<TransitionError> for LedgerError {
    fn from(e: TransitionError) -> Self {
        LedgerError::Transition(e)
    }
}

/// What opening a log found and repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// Records replayed.
    pub records: u64,
    /// Bytes of a torn final record that were cut off.
    pub torn_bytes: u64,
    /// Dispatches that were in `delivering` and are now `delivery_unknown`.
    pub moved_to_delivery_unknown: Vec<DispatchId>,
}

/// A new dispatch.
#[derive(Debug, Clone)]
pub struct NewDispatch {
    pub id: DispatchId,
    pub content: ContentRef,
    pub owner: SessionId,
    pub created_by: Code,
    pub depends_on: Vec<DispatchId>,
    /// Redeliveries allowed after the first. At most `MAX_RETRIES_LIMIT`;
    /// a larger value is refused with `MaxRetriesTooLarge`.
    pub max_retries: u32,
}

/// What became of a worker record. A rejected record is logged, not dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    Accepted(Outcome),
    Rejected(RejectReason),
}

/// The ledger over a [`Store`].
pub struct Ledger<S: Store> {
    store: S,
    state: LedgerState,
    next_seq: u64,
    clock: Clock,
    failed: bool,
}

impl<S: Store> Ledger<S> {
    /// Replay the log, cut off a torn final record and move every dispatch
    /// left in `delivering` to `delivery_unknown`. A log that does not replay
    /// is an error: the ledger does not guess.
    pub fn open(mut store: S, clock: Clock) -> Result<(Self, Recovery), LedgerError> {
        let bytes = store.load().map_err(LedgerError::Open)?;
        let replayed = replay(&bytes).map_err(LedgerError::Replay)?;
        if replayed.torn_bytes > 0 {
            store
                .truncate(replayed.valid_len)
                .map_err(LedgerError::Open)?;
        }
        let mut ledger = Ledger {
            store,
            state: replayed.state,
            next_seq: replayed.next_seq,
            clock,
            failed: false,
        };
        let mut moved = Vec::new();
        for (id, attempt, request_id) in ledger.state.delivering() {
            let at = (ledger.clock)();
            ledger.commit(Event::DeliveryUnknown {
                id: id.clone(),
                attempt,
                request_id,
                reason: state::recovered_code(),
                at,
            })?;
            moved.push(id);
        }
        let recovery = Recovery {
            records: replayed.records,
            torn_bytes: replayed.torn_bytes,
            moved_to_delivery_unknown: moved,
        };
        Ok((ledger, recovery))
    }

    pub fn get(&self, id: &DispatchId) -> Option<&Dispatch> {
        self.state.get(id)
    }

    pub fn state(&self) -> &LedgerState {
        &self.state
    }

    /// Whether a write failed and the ledger now refuses writes.
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Log one event, then apply it. The event is validated first, so a
    /// refused transition writes nothing; the log is written before memory
    /// changes, so a failed write leaves the state as it was.
    fn commit(&mut self, event: Event) -> Result<(), LedgerError> {
        if self.failed {
            return Err(LedgerError::LogUnavailable);
        }
        self.state.validate(&event)?;
        let record = encode_record(self.next_seq, &event);
        if let Err(e) = self.store.append(&record) {
            self.failed = true;
            return Err(LedgerError::Store(e));
        }
        self.next_seq += 1;
        self.state.apply(&event)?;
        Ok(())
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn find(&self, id: &DispatchId) -> Result<&Dispatch, LedgerError> {
        self.state
            .get(id)
            .ok_or_else(|| TransitionError::UnknownDispatch(id.clone()).into())
    }

    /// Create a dispatch. Each call appends a record and syncs, so a caller
    /// exposed to untrusted senders must limit how often it calls. Retention
    /// and compaction of the log are out of scope here.
    ///
    /// A dispatch is `ready` if every dependency is `done`, else `pending`.
    pub fn create(&mut self, new: NewDispatch) -> Result<(), LedgerError> {
        let at = self.now();
        self.commit(Event::Created {
            id: new.id,
            content: new.content,
            owner: new.owner,
            created_by: new.created_by,
            depends_on: new.depends_on,
            max_retries: new.max_retries,
            at,
        })
    }

    /// Log the intent to inject, before the floor request. Returns the attempt
    /// number. If no request is left, the dispatch is set to `failed` and the
    /// call returns `RetryCapReached`: the state changed.
    ///
    /// The attempt number is never reused, which is what lets a worker record
    /// be matched on (dispatch, attempt, session) alone. A dispatch also never
    /// reuses a request id: a repeat is refused with `RequestReused` and
    /// nothing is written, so a late answer to an earlier request cannot be
    /// taken for the answer to this one.
    /// The caller asks the runtime only after this returns `Ok`.
    pub fn begin_delivery(
        &mut self,
        id: &DispatchId,
        request_id: Code,
    ) -> Result<u32, LedgerError> {
        let d = self.find(id)?;
        let at = self.now();
        // Refused requests do not use the retry budget, so the counter itself
        // is the only bound on them. At u32::MAX no request number is left:
        // a `ready` dispatch is logged as `failed`, like any dispatch with no
        // request left.
        let Some(attempt) = d.attempt.checked_add(1) else {
            if d.state == State::Ready {
                return self.fail_with(id, "request_counter_exhausted", at);
            }
            return Err(TransitionError::RetryCapReached(id.clone()).into());
        };
        if d.state == State::Ready && d.spent() + 1 > u64::from(d.max_retries) + 1 {
            return self.fail_at_cap(id, at);
        }
        self.commit(Event::DeliveryStarted {
            id: id.clone(),
            attempt,
            request_id,
            at,
        })?;
        Ok(attempt)
    }

    /// The runtime confirmed the injection of `request_id`. Also resolves
    /// `delivery_unknown` when the runtime's audit shows that request was
    /// injected. An answer naming any other request is refused, so a late
    /// answer to an earlier request cannot move a later one.
    pub fn confirm_delivered(
        &mut self,
        id: &DispatchId,
        request_id: &Code,
    ) -> Result<(), LedgerError> {
        let attempt = self.find(id)?.attempt;
        let at = self.now();
        self.commit(Event::Delivered {
            id: id.clone(),
            attempt,
            request_id: request_id.clone(),
            at,
        })
    }

    /// The runtime refused `request_id`. Nothing was injected: back to
    /// `ready`. The attempt number stays used, so the next request gets a new
    /// one, but the refusal does not count against the retry cap.
    pub fn refuse_delivery(
        &mut self,
        id: &DispatchId,
        request_id: &Code,
        reason: Code,
    ) -> Result<(), LedgerError> {
        let attempt = self.find(id)?.attempt;
        let at = self.now();
        self.commit(Event::DeliveryRefused {
            id: id.clone(),
            attempt,
            request_id: request_id.clone(),
            reason,
            at,
        })
    }

    /// The answer to `request_id` is lost (a timeout): `delivering` to
    /// `delivery_unknown`.
    pub fn mark_delivery_unknown(
        &mut self,
        id: &DispatchId,
        request_id: &Code,
        reason: Code,
    ) -> Result<(), LedgerError> {
        let attempt = self.find(id)?.attempt;
        let at = self.now();
        self.commit(Event::DeliveryUnknown {
            id: id.clone(),
            attempt,
            request_id: request_id.clone(),
            reason,
            at,
        })
    }

    /// The runtime's audit shows `request_id` was not injected:
    /// `delivery_unknown` to `ready`. Only this call can do it. The attempt
    /// number stays used and the retry cap is not charged.
    pub fn resolve_not_injected(
        &mut self,
        id: &DispatchId,
        request_id: &Code,
    ) -> Result<(), LedgerError> {
        let attempt = self.find(id)?.attempt;
        let at = self.now();
        self.commit(Event::DeliveryNotInjected {
            id: id.clone(),
            attempt,
            request_id: request_id.clone(),
            at,
        })
    }

    /// The turn ended without a record: `delivered` to `ready`, if a retry is
    /// left. At the cap the dispatch is moved to `failed` (reason
    /// `retry_cap_reached`) and the call returns `RetryCapReached`.
    pub fn request_retry(&mut self, id: &DispatchId) -> Result<(), LedgerError> {
        let d = self.find(id)?;
        let attempt = d.attempt;
        let at_cap = d.state == State::Delivered && d.spent() > u64::from(d.max_retries);
        let at = self.now();
        if at_cap {
            return self.fail_at_cap(id, at);
        }
        self.commit(Event::RetryRequested {
            id: id.clone(),
            attempt,
            at,
        })
    }

    /// Log `failed` for a dispatch that used its attempts, then report it.
    fn fail_at_cap<T>(&mut self, id: &DispatchId, at: u64) -> Result<T, LedgerError> {
        self.fail_with(id, "retry_cap_reached", at)
    }

    fn fail_with<T>(&mut self, id: &DispatchId, reason: &str, at: u64) -> Result<T, LedgerError> {
        self.commit(Event::Failed {
            id: id.clone(),
            reason: Code::new(reason).expect("a fixed valid code"),
            at,
        })?;
        Err(TransitionError::RetryCapReached(id.clone()).into())
    }

    /// An operator returns a `blocked` dispatch to `ready`.
    pub fn unblock(&mut self, id: &DispatchId) -> Result<(), LedgerError> {
        let at = self.now();
        self.commit(Event::Unblocked { id: id.clone(), at })
    }

    /// Give up on a dispatch.
    pub fn fail(&mut self, id: &DispatchId, reason: Code) -> Result<(), LedgerError> {
        let at = self.now();
        self.commit(Event::Failed {
            id: id.clone(),
            reason,
            at,
        })
    }

    /// Run the matching rule on a worker record and log the result: a
    /// `record_accepted` event that moves the dispatch, or a `record_rejected`
    /// event that moves nothing. A record is never dropped without a trace; if
    /// the log cannot be written the call returns the error and the state is
    /// unchanged.
    ///
    /// Every call appends a record, including for claims that match nothing,
    /// so the caller must apply a per-session rate limit or dedup before this.
    pub fn submit_record(&mut self, claim: Claim) -> Result<RecordOutcome, LedgerError> {
        let at = self.now();
        let id = DispatchId::new(claim.dispatch_id.as_str()).ok();
        let session = SessionId::new(claim.session.as_str()).ok();
        let verdict = match_record(id.as_ref().and_then(|id| self.state.get(id)), &claim);
        match (verdict, id, session) {
            (Ok(outcome), Some(id), Some(session)) => {
                // The match succeeded, so the attempt equals the open one,
                // which is a u32.
                let attempt = u32::try_from(claim.attempt).expect("a matched attempt is a u32");
                self.commit(Event::RecordAccepted {
                    id,
                    attempt,
                    session,
                    outcome,
                    summary: claim.summary,
                    at,
                })?;
                Ok(RecordOutcome::Accepted(outcome))
            }
            (verdict, _, _) => {
                let reason = verdict.err().unwrap_or(RejectReason::UnknownDispatch);
                self.commit(Event::RecordRejected {
                    claimed_id: cap(&claim.dispatch_id),
                    claimed_attempt: claim.attempt,
                    claimed_session: cap(&claim.session),
                    reason,
                    at,
                })?;
                Ok(RecordOutcome::Rejected(reason))
            }
        }
    }
}

/// Keep at most [`MAX_NAME_LEN`] characters of an untrusted string.
fn cap(s: &str) -> String {
    s.chars().take(MAX_NAME_LEN).collect()
}
