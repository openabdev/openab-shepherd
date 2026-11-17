//! The state machine and the matching rule. Pure: no I/O, no clock.
//!
//! `validate` says whether an event may follow the current state, `apply`
//! changes the state. Replay and the live path both go through them, so the
//! two cannot disagree.

use super::event::Event;
use super::types::{Code, ContentRef, Dispatch, DispatchId, Outcome, RejectReason, State};
use std::collections::BTreeMap;
use std::fmt;

/// A worker record as received: every field is the sender's claim until the
/// matching rule has run. `session` is the identity the transport stamped, not
/// anything from the message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub dispatch_id: String,
    pub attempt: u64,
    pub session: String,
    pub outcome: Outcome,
    /// Hash and length of the optional summary. The text is not kept.
    pub summary: Option<ContentRef>,
}

/// The matching rule for a worker record, as a pure function.
///
/// A record counts only if it names this dispatch and its open attempt, comes
/// from the session the dispatch was delivered to, finds the dispatch in
/// `delivered`, and no accepted record closed that attempt before.
///
/// The record is bound to (dispatch, attempt, session), not to a request id.
/// That is sound only because attempt numbers are never reused: a refused or
/// not-injected request keeps its number. Changing that breaks this rule. The checks
/// run in a fixed order; the first failure is the reason returned.
pub fn match_record(dispatch: Option<&Dispatch>, claim: &Claim) -> Result<Outcome, RejectReason> {
    let d = dispatch.ok_or(RejectReason::UnknownDispatch)?;
    if claim.dispatch_id != d.id.as_str() {
        return Err(RejectReason::UnknownDispatch);
    }
    if claim.session != d.owner.as_str() {
        return Err(RejectReason::WrongSession);
    }
    let closed = u32::try_from(claim.attempt).is_ok_and(|a| d.closed_attempts.contains(&a));
    if closed || d.state == State::Done {
        return Err(RejectReason::AlreadyClosed);
    }
    let open = u64::from(d.attempt);
    if claim.attempt < open {
        return Err(RejectReason::StaleAttempt);
    }
    if claim.attempt > open {
        return Err(RejectReason::FutureAttempt);
    }
    if d.state != State::Delivered {
        return Err(RejectReason::NotDelivered);
    }
    Ok(claim.outcome)
}

/// The largest accepted `max_retries`. It keeps `max_retries + 1` attempts
/// below `u32::MAX`, so the attempt counter can always reach the cap and the
/// dispatch can always be moved to `failed` there.
pub const MAX_RETRIES_LIMIT: u32 = u32::MAX - 2;

/// An event does not fit the current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    DuplicateId(DispatchId),
    UnknownDispatch(DispatchId),
    UnknownDependency(DispatchId),
    SelfDependency,
    DuplicateDependency(DispatchId),
    WrongState {
        id: DispatchId,
        state: State,
    },
    WrongAttempt {
        id: DispatchId,
        expected: u32,
        got: u32,
    },
    /// The answer names a request other than the one logged for the open
    /// attempt: a late or misrouted runtime answer.
    WrongRequest {
        id: DispatchId,
        expected: Option<Code>,
        got: Code,
    },
    /// The retry cap is reached. The caller must fail the dispatch.
    RetryCapReached(DispatchId),
    /// The dispatch already used this request id for an earlier attempt.
    RequestReused {
        id: DispatchId,
        request_id: Code,
    },
    /// `max_retries` is above `MAX_RETRIES_LIMIT`.
    MaxRetriesTooLarge(u32),
    /// A `record_accepted` event that the matching rule would reject.
    RecordNotMatching(RejectReason),
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionError::DuplicateId(id) => write!(f, "dispatch {id} already exists"),
            TransitionError::UnknownDispatch(id) => write!(f, "no dispatch {id}"),
            TransitionError::UnknownDependency(id) => write!(f, "unknown dependency {id}"),
            TransitionError::SelfDependency => f.write_str("a dispatch cannot depend on itself"),
            TransitionError::DuplicateDependency(id) => write!(f, "dependency {id} listed twice"),
            TransitionError::WrongState { id, state } => {
                write!(f, "dispatch {id} is {state}, which does not allow this")
            }
            TransitionError::WrongAttempt { id, expected, got } => {
                write!(f, "dispatch {id}: attempt {got}, expected {expected}")
            }
            TransitionError::WrongRequest { id, got, .. } => {
                write!(f, "dispatch {id}: request {got} is not the open request")
            }
            TransitionError::RetryCapReached(id) => {
                write!(f, "dispatch {id} has used its retries")
            }
            TransitionError::RequestReused { id, request_id } => {
                write!(f, "dispatch {id} already used request {request_id}")
            }
            TransitionError::MaxRetriesTooLarge(n) => {
                write!(f, "max_retries {n} is above {MAX_RETRIES_LIMIT}")
            }
            TransitionError::RecordNotMatching(r) => write!(f, "record does not match: {r}"),
        }
    }
}

impl std::error::Error for TransitionError {}

/// Every dispatch, derived from the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerState {
    dispatches: BTreeMap<DispatchId, Dispatch>,
}

impl LedgerState {
    pub fn get(&self, id: &DispatchId) -> Option<&Dispatch> {
        self.dispatches.get(id)
    }

    /// All dispatches, ordered by id.
    pub fn iter(&self) -> impl Iterator<Item = &Dispatch> {
        self.dispatches.values()
    }

    pub fn len(&self) -> usize {
        self.dispatches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dispatches.is_empty()
    }

    fn find(&self, id: &DispatchId) -> Result<&Dispatch, TransitionError> {
        self.dispatches
            .get(id)
            .ok_or_else(|| TransitionError::UnknownDispatch(id.clone()))
    }

    fn deps_done(&self, depends_on: &[DispatchId]) -> bool {
        depends_on.iter().all(|d| {
            self.dispatches
                .get(d)
                .is_some_and(|d| d.state == State::Done)
        })
    }

    /// Whether `event` may follow the current state. Changes nothing.
    pub fn validate(&self, event: &Event) -> Result<(), TransitionError> {
        match event {
            Event::Created {
                id,
                depends_on,
                max_retries,
                ..
            } => {
                if *max_retries > MAX_RETRIES_LIMIT {
                    return Err(TransitionError::MaxRetriesTooLarge(*max_retries));
                }
                if self.dispatches.contains_key(id) {
                    return Err(TransitionError::DuplicateId(id.clone()));
                }
                // Every dependency must already exist and cannot be the new
                // id, so a cycle cannot be written.
                for (i, dep) in depends_on.iter().enumerate() {
                    if dep == id {
                        return Err(TransitionError::SelfDependency);
                    }
                    if depends_on[..i].contains(dep) {
                        return Err(TransitionError::DuplicateDependency(dep.clone()));
                    }
                    if !self.dispatches.contains_key(dep) {
                        return Err(TransitionError::UnknownDependency(dep.clone()));
                    }
                }
                Ok(())
            }
            Event::DeliveryStarted {
                id,
                attempt,
                request_id,
                ..
            } => {
                let d = self.find(id)?;
                expect_state(d, &[State::Ready])?;
                let next = d
                    .attempt
                    .checked_add(1)
                    .ok_or_else(|| TransitionError::RetryCapReached(id.clone()))?;
                if *attempt != next {
                    return Err(wrong_attempt(d, next, *attempt));
                }
                if u64::from(*attempt) > u64::from(d.max_retries) + 1 {
                    return Err(TransitionError::RetryCapReached(id.clone()));
                }
                if d.used_requests.contains(request_id) {
                    return Err(TransitionError::RequestReused {
                        id: id.clone(),
                        request_id: request_id.clone(),
                    });
                }
                Ok(())
            }
            Event::Delivered {
                id,
                attempt,
                request_id,
                ..
            } => {
                let d = self.find(id)?;
                expect_state(d, &[State::Delivering, State::DeliveryUnknown])?;
                expect_attempt(d, *attempt)?;
                expect_request(d, request_id)
            }
            Event::DeliveryRefused {
                id,
                attempt,
                request_id,
                ..
            }
            | Event::DeliveryUnknown {
                id,
                attempt,
                request_id,
                ..
            } => {
                let d = self.find(id)?;
                expect_state(d, &[State::Delivering])?;
                expect_attempt(d, *attempt)?;
                expect_request(d, request_id)
            }
            Event::DeliveryNotInjected {
                id,
                attempt,
                request_id,
                ..
            } => {
                let d = self.find(id)?;
                expect_state(d, &[State::DeliveryUnknown])?;
                expect_attempt(d, *attempt)?;
                expect_request(d, request_id)
            }
            Event::RetryRequested { id, attempt, .. } => {
                let d = self.find(id)?;
                expect_state(d, &[State::Delivered])?;
                expect_attempt(d, *attempt)?;
                if d.attempt > d.max_retries {
                    return Err(TransitionError::RetryCapReached(id.clone()));
                }
                Ok(())
            }
            Event::RecordAccepted {
                id,
                attempt,
                session,
                outcome,
                summary,
                ..
            } => {
                let claim = Claim {
                    dispatch_id: id.to_string(),
                    attempt: u64::from(*attempt),
                    session: session.to_string(),
                    outcome: *outcome,
                    summary: summary.clone(),
                };
                match_record(self.dispatches.get(id), &claim)
                    .map(|_| ())
                    .map_err(TransitionError::RecordNotMatching)
            }
            Event::RecordRejected { .. } => Ok(()),
            Event::Unblocked { id, .. } => {
                let d = self.find(id)?;
                expect_state(d, &[State::Blocked])
            }
            Event::Failed { id, .. } => {
                let d = self.find(id)?;
                expect_state(
                    d,
                    &[
                        State::Pending,
                        State::Ready,
                        State::Delivered,
                        State::DeliveryUnknown,
                        State::Blocked,
                    ],
                )
            }
        }
    }

    /// Validate `event`, then apply it. Nothing changes on an error.
    pub fn apply(&mut self, event: &Event) -> Result<(), TransitionError> {
        self.validate(event)?;
        match event {
            Event::Created {
                id,
                content,
                owner,
                created_by,
                depends_on,
                max_retries,
                at,
            } => {
                let state = if self.deps_done(depends_on) {
                    State::Ready
                } else {
                    State::Pending
                };
                self.dispatches.insert(
                    id.clone(),
                    Dispatch {
                        id: id.clone(),
                        content: content.clone(),
                        owner: owner.clone(),
                        created_by: created_by.clone(),
                        depends_on: depends_on.clone(),
                        state,
                        attempt: 0,
                        max_retries: *max_retries,
                        request_id: None,
                        used_requests: Default::default(),
                        closed_attempts: Default::default(),
                        outcome: None,
                        failure: None,
                        created_at: *at,
                        updated_at: *at,
                        delivered_at: None,
                    },
                );
            }
            Event::DeliveryStarted {
                id,
                attempt,
                request_id,
                at,
            } => {
                let d = self.get_mut(id, *at);
                d.state = State::Delivering;
                d.attempt = *attempt;
                d.request_id = Some(request_id.clone());
                d.used_requests.insert(request_id.clone());
            }
            Event::Delivered { id, at, .. } => {
                let d = self.get_mut(id, *at);
                d.state = State::Delivered;
                d.delivered_at = Some(*at);
            }
            Event::DeliveryRefused { id, at, .. } | Event::DeliveryNotInjected { id, at, .. } => {
                // Nothing reached the worker. The attempt number stays used: a
                // later request never shares it, so a late answer to this one
                // cannot be taken for the next.
                self.get_mut(id, *at).state = State::Ready;
            }
            Event::DeliveryUnknown { id, at, .. } => {
                self.get_mut(id, *at).state = State::DeliveryUnknown;
            }
            Event::RetryRequested { id, at, .. } => {
                self.get_mut(id, *at).state = State::Ready;
            }
            Event::RecordAccepted {
                id,
                attempt,
                outcome,
                at,
                ..
            } => {
                let d = self.get_mut(id, *at);
                d.closed_attempts.insert(*attempt);
                d.outcome = Some(*outcome);
                d.state = match outcome {
                    Outcome::Done => State::Done,
                    Outcome::Blocked | Outcome::Cannot => State::Blocked,
                };
                if *outcome == Outcome::Done {
                    self.release_dependents(*at);
                }
            }
            Event::RecordRejected { .. } => {}
            Event::Unblocked { id, at } => {
                let d = self.get_mut(id, *at);
                d.state = State::Ready;
                d.outcome = None;
            }
            Event::Failed { id, reason, at } => {
                let d = self.get_mut(id, *at);
                d.state = State::Failed;
                d.failure = Some(reason.clone());
            }
        }
        Ok(())
    }

    /// `validate` ran first, so the id exists.
    fn get_mut(&mut self, id: &DispatchId, at: u64) -> &mut Dispatch {
        let d = self
            .dispatches
            .get_mut(id)
            .expect("validated: the dispatch exists");
        d.updated_at = at;
        d
    }

    fn release_dependents(&mut self, at: u64) {
        let ready: Vec<DispatchId> = self
            .dispatches
            .values()
            .filter(|d| d.state == State::Pending && self.deps_done(&d.depends_on))
            .map(|d| d.id.clone())
            .collect();
        for id in ready {
            let d = self.get_mut(&id, at);
            d.state = State::Ready;
        }
    }

    /// Ids in `delivering`, ordered.
    pub(crate) fn delivering(&self) -> Vec<(DispatchId, u32, Code)> {
        self.dispatches
            .values()
            .filter(|d| d.state == State::Delivering)
            .filter_map(|d| Some((d.id.clone(), d.attempt, d.request_id.clone()?)))
            .collect()
    }
}

fn expect_state(d: &Dispatch, allowed: &[State]) -> Result<(), TransitionError> {
    if allowed.contains(&d.state) {
        Ok(())
    } else {
        Err(TransitionError::WrongState {
            id: d.id.clone(),
            state: d.state,
        })
    }
}

fn expect_attempt(d: &Dispatch, got: u32) -> Result<(), TransitionError> {
    if got == d.attempt {
        Ok(())
    } else {
        Err(wrong_attempt(d, d.attempt, got))
    }
}

fn expect_request(d: &Dispatch, got: &Code) -> Result<(), TransitionError> {
    if d.request_id.as_ref() == Some(got) {
        Ok(())
    } else {
        Err(TransitionError::WrongRequest {
            id: d.id.clone(),
            expected: d.request_id.clone(),
            got: got.clone(),
        })
    }
}

fn wrong_attempt(d: &Dispatch, expected: u32, got: u32) -> TransitionError {
    TransitionError::WrongAttempt {
        id: d.id.clone(),
        expected,
        got,
    }
}

/// The code recorded when the ledger itself moves a dispatch out of `delivering`.
pub(crate) fn recovered_code() -> Code {
    Code::new("recovered_after_restart").expect("a fixed valid code")
}
