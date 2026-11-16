//! A person at a native pane: the input lease, the draft flag, and the
//! refusals a bot meets while either exists (issue #77, ADR
//! restart-survival-and-shared-panes §4).
//!
//! Nothing here is reachable from the tools yet: no client can take a lease
//! until the attach surface lands, so a bot's write adds only two "none"
//! tests inside the existing critical section. The API is `pub(crate)` and
//! is what that surface will call.
//!
//! Lock order, once: the tools layer's `PaneLocks` (the caller's), then
//! `Pane.input`, then `Pane.state`, then the `MetaStore`. Every operation here
//! that changes the lease or the draft holds `input`, so none lands between a
//! bot's paste and its Enter. A meta write is never made while `state` is
//! held.

#![cfg_attr(not(test), allow(dead_code))] // the shared-panes attach calls these

use super::*;
use crate::shared_panes::{HumanState, LeaseInfo, SharedPanes};
use std::sync::TryLockError;

/// How long a lease operation waits for a bot's in-flight write (a paste, its
/// settle and its Enter) before it gives up. A std `Mutex` has no timeout, so
/// it is polled.
const INPUT_WAIT: Duration = Duration::from_secs(3);
const INPUT_POLL: Duration = Duration::from_millis(2);

/// A lease just granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseGrant {
    pub id: u64,
    pub expires_in: Duration,
}

/// Where one lease stands, for the thread that serves its holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseStatus {
    Held {
        idle: Duration,
        expires_in: Duration,
    },
    /// It ended for want of input.
    Idle,
    /// It was released or taken over.
    Lost,
}

fn whole_secs_up(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

impl Pane {
    /// Wait for `input` for at most `wait`.
    fn lock_input_within(&self, wait: Duration) -> Result<MutexGuard<'_, ()>, BackendError> {
        let end = Instant::now() + wait;
        loop {
            match self.input.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(_)) => return Err(self.poisoned()),
                Err(TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= end {
                return Err(BackendError::new(
                    ErrorKind::Timeout,
                    format!("native: {}: the pane is busy with another write", self.id),
                ));
            }
            thread::sleep(INPUT_POLL);
        }
    }

    /// The lease, if one is held and has not idled out.
    fn live_lease<'a>(&self, st: &'a PaneState) -> Option<&'a Lease> {
        let now = self.clock.now();
        st.lease.as_ref().filter(|lease| !lease.expired(now))
    }

    /// A bot's refusal while a person is at the pane: the lease first (the
    /// draft belongs to the person who is there), then the draft. Nothing is
    /// written and nothing is kept.
    pub(super) fn refuse_while_human(&self, st: &PaneState) -> Result<(), BackendError> {
        if let Some(lease) = self.live_lease(st) {
            let now = self.clock.now();
            return Err(BackendError::new(
                ErrorKind::HumanControlled {
                    idle_secs: lease.idle(now).as_secs(),
                    expires_in_secs: whole_secs_up(lease.expires_in(now)),
                },
                format!(
                    "native: {}: a person holds the input lease; nothing was written",
                    self.id
                ),
            ));
        }
        if st.draft {
            return Err(BackendError::new(
                ErrorKind::DraftPending,
                format!(
                    "native: {}: a person has typed into the pane and not pressed Enter; \
                     nothing was written",
                    self.id
                ),
            ));
        }
        Ok(())
    }

    /// A person's write needs the lease it names, not idled out.
    pub(super) fn check_lease(&self, st: &PaneState, lease_id: u64) -> Result<(), BackendError> {
        match self.lease_status_of(st, lease_id) {
            LeaseStatus::Held { .. } => Ok(()),
            LeaseStatus::Idle => Err(lease_lost(&self.id, "it idled out")),
            LeaseStatus::Lost => Err(lease_lost(&self.id, "it was released or taken over")),
        }
    }

    fn lease_status_of(&self, st: &PaneState, lease_id: u64) -> LeaseStatus {
        let now = self.clock.now();
        match &st.lease {
            Some(lease) if lease.id == lease_id && lease.expired(now) => LeaseStatus::Idle,
            Some(lease) if lease.id == lease_id => LeaseStatus::Held {
                idle: lease.idle(now),
                expires_in: lease.expires_in(now),
            },
            _ => LeaseStatus::Lost,
        }
    }

    fn set_draft_flag(&self, value: bool) -> Result<(), BackendError> {
        self.lock_state()?.draft = value;
        Ok(())
    }

    /// Whether the meta (the last thing the host was asked to hold) already
    /// has the flag set. True with no meta store, where there is nothing to
    /// write.
    fn meta_agrees_draft_set(&self) -> bool {
        self.meta.as_ref().is_none_or(|meta| meta.get().human_draft)
    }

    fn record_draft(&self) -> Result<(), BackendError> {
        let Some(meta) = &self.meta else {
            return Ok(());
        };
        meta.set_human_draft().map_err(|err| {
            BackendError::new(
                ErrorKind::Other,
                format!("native: {}: cannot record the human input: {err}", self.id),
            )
        })
    }

    fn record_no_draft(&self) -> Result<(), BackendError> {
        let Some(meta) = &self.meta else {
            return Ok(());
        };
        meta.clear_human_draft().map_err(|err| {
            BackendError::new(
                ErrorKind::Other,
                format!(
                    "native: {}: cannot record the cleared draft: {err}",
                    self.id
                ),
            )
        })
    }
}

fn lease_lost(id: &str, why: &str) -> BackendError {
    BackendError::new(
        ErrorKind::InvalidInput,
        format!("native: {id}: the input lease is gone ({why}); nothing was written"),
    )
}

impl NativeBackend {
    /// Take the pane's input lease for `holder`. Waits (bounded) behind any
    /// write in flight, so it never lands between a bot's paste and its Enter.
    ///
    /// A lease held by someone else, not idled out, is refused unless
    /// `preempt`. The same holder asking again keeps its lease and renews it.
    /// Acquiring does not touch the draft.
    pub(crate) fn lease_acquire(
        &self,
        t: &TerminalRef,
        holder: &str,
        preempt: bool,
    ) -> Result<LeaseGrant, BackendError> {
        let pane = self.inner.resolve(t)?;
        let _input = pane.lock_input_within(INPUT_WAIT)?;
        let mut st = pane.lock_state()?;
        if !st.live {
            return Err(finished(&pane.id));
        }
        let now = pane.clock.now();
        match st.lease.as_mut() {
            Some(lease) if !lease.expired(now) && lease.holder == holder => {
                lease.last_input = now;
            }
            Some(lease) if !lease.expired(now) && !preempt => {
                return Err(BackendError::new(
                    ErrorKind::HumanControlled {
                        idle_secs: lease.idle(now).as_secs(),
                        expires_in_secs: whole_secs_up(lease.expires_in(now)),
                    },
                    format!("native: {}: the input lease is held", pane.id),
                ));
            }
            _ => st.lease = Some(Lease::new(holder.to_string(), now)),
        }
        let lease = st.lease.as_ref().expect("the lease was just set");
        Ok(LeaseGrant {
            id: lease.id,
            expires_in: lease.expires_in(now),
        })
    }

    /// End lease `lease_id`. True when it was the held one. The draft stays.
    pub(crate) fn lease_release(
        &self,
        t: &TerminalRef,
        lease_id: u64,
    ) -> Result<bool, BackendError> {
        let pane = self.inner.resolve(t)?;
        let mut st = pane.lock_state()?;
        let held = st.lease.as_ref().is_some_and(|lease| lease.id == lease_id);
        if held {
            st.lease = None;
        }
        Ok(held)
    }

    /// Where lease `lease_id` stands.
    pub(crate) fn lease_status(
        &self,
        t: &TerminalRef,
        lease_id: u64,
    ) -> Result<LeaseStatus, BackendError> {
        let pane = self.inner.resolve(t)?;
        let st = pane.lock_state()?;
        Ok(pane.lease_status_of(&st, lease_id))
    }

    /// The lease and the draft, as a status read sees them.
    pub(crate) fn human_state(&self, t: &TerminalRef) -> Result<HumanState, BackendError> {
        let pane = self.inner.resolve(t)?;
        let st = pane.lock_state()?;
        let now = pane.clock.now();
        Ok(HumanState {
            lease: pane.live_lease(&st).map(|lease| LeaseInfo {
                holder: lease.holder.clone(),
                idle: lease.idle(now),
                expires_in: lease.expires_in(now),
            }),
            draft: st.draft,
        })
    }

    /// A person's bytes into the pane, under lease `lease_id`.
    ///
    /// Any byte sets the draft, in this order: the host has acknowledged the
    /// flag, then the flag is set here, then the byte is written. A crash in
    /// between leaves the flag set and the pane untouched, never bytes without
    /// the flag. An Enter (see [`lease::EnterScanner`]) clears it again, flag
    /// in the host first, unless something was typed after the last Enter in
    /// the same call; if the host does not acknowledge the clear the flag
    /// stays set and the next Enter (or `draft_clear`) retries. Not guarded
    /// by the program's status: a person may type into a replayed pane.
    ///
    /// Accepted input renews the lease; a lease that idled out or was taken
    /// is refused and nothing is written.
    pub(crate) fn human_write(
        &self,
        t: &TerminalRef,
        lease_id: u64,
        bytes: &[u8],
    ) -> Result<(), BackendError> {
        if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "native: human input must be 1 to {MAX_INPUT_BYTES} bytes"
            )));
        }
        let pane = self.inner.resolve(t)?;
        let _input = pane.lock_input()?;
        let (scan, scanner, flag_set) = {
            let st = pane.lock_state()?;
            if !st.live {
                return Err(finished(&pane.id));
            }
            pane.check_lease(&st, lease_id)?;
            let mut scanner = st
                .lease
                .as_ref()
                .map(|lease| lease.scan.clone())
                .unwrap_or_default();
            (scanner.scan(bytes), scanner, st.draft)
        };
        // The flag is in the host before the byte is written. Skipped only
        // when both copies already say so.
        if !(flag_set && pane.meta_agrees_draft_set()) {
            pane.record_draft()?;
        }
        pane.set_draft_flag(true)?;
        pane.write_guarded(
            WriteOrigin::Human { lease_id },
            None,
            |_| Ok(bytes.to_vec()),
        )?;
        {
            let mut st = pane.lock_state()?;
            let now = pane.clock.now();
            if let Some(lease) = st.lease.as_mut().filter(|lease| lease.id == lease_id) {
                lease.last_input = now;
                lease.scan = scanner;
            }
        }
        if scan.enter && !scan.trailing {
            match pane.record_no_draft() {
                Ok(()) => pane.set_draft_flag(false)?,
                // The byte went; the flag stays, the safe side.
                Err(err) => eprintln!("openab-shepherd: {err}"),
            }
        }
        Ok(())
    }

    /// Clear the draft, whoever holds the lease, and leave the lease alone.
    /// True when a draft was set. Waits (bounded) behind a write in flight.
    pub(crate) fn draft_clear(&self, t: &TerminalRef) -> Result<bool, BackendError> {
        let pane = self.inner.resolve(t)?;
        let _input = pane.lock_input_within(INPUT_WAIT)?;
        let was_set = pane.lock_state()?.draft;
        // A flag the host holds for a byte that never went (the write failed
        // after the host acknowledged it) is cleared too.
        let host_has = pane
            .meta
            .as_ref()
            .is_some_and(|meta| meta.get().human_draft);
        if was_set || host_has {
            pane.record_no_draft()?;
        }
        if was_set {
            pane.set_draft_flag(false)?;
        }
        Ok(was_set)
    }
}

impl SharedPanes for NativeBackend {
    fn human_state(&self, t: &TerminalRef) -> Result<HumanState, BackendError> {
        NativeBackend::human_state(self, t)
    }

    fn draft_clear(&self, t: &TerminalRef) -> Result<bool, BackendError> {
        NativeBackend::draft_clear(self, t)
    }
}
