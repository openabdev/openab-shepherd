//! The vocabulary of the ledger: validated names, the content reference, the
//! dispatch states and the dispatch record.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

/// The longest accepted identifier, in bytes.
pub const MAX_NAME_LEN: usize = 128;

/// An identifier was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    /// Only ASCII letters, digits and `. _ : -` are accepted.
    BadChar,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NameError::Empty => "name is empty",
            NameError::TooLong => "name is longer than 128 bytes",
            NameError::BadChar => "name has a character outside letters, digits and . _ : -",
        })
    }
}

fn check_name(s: &str) -> Result<(), NameError> {
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    if s.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
    {
        return Err(NameError::BadChar);
    }
    Ok(())
}

macro_rules! name_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, NameError> {
                let value = value.into();
                check_name(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_type!(
    /// The opaque, unique id of a dispatch. Stable across retries.
    DispatchId
);
name_type!(
    /// The identity of a worker session, as the transport reports it.
    SessionId
);
name_type!(
    /// A short machine code: who created a dispatch, why a step failed,
    /// a request id. Never free text.
    Code
);

/// The content reference of a dispatch: a SHA-256 and a length. The ledger
/// never stores the text itself, so a secret typed into a dispatch is not
/// copied into the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentRef {
    sha256: String,
    len: u64,
}

/// A content reference was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadContentRef;

impl fmt::Display for BadContentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256 must be 64 lowercase hex digits")
    }
}

impl ContentRef {
    /// Hash `text`. The text is not kept.
    pub fn of(text: &str) -> Self {
        let digest = Sha256::digest(text.as_bytes());
        let mut sha256 = String::with_capacity(64);
        for byte in digest {
            sha256.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            sha256.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
        }
        Self {
            sha256,
            len: text.len() as u64,
        }
    }

    /// A reference to content hashed elsewhere.
    pub fn new(sha256: impl Into<String>, len: u64) -> Result<Self, BadContentRef> {
        let sha256 = sha256.into();
        if sha256.len() != 64
            || !sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(BadContentRef);
        }
        Ok(Self { sha256, len })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Where a dispatch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    /// Created, and at least one dependency is not `done`.
    Pending,
    /// Every dependency is `done`. May be delivered.
    Ready,
    /// The intent to inject was logged; the answer is not.
    Delivering,
    /// The runtime confirmed the injection. Waiting for a worker record.
    Delivered,
    /// A crash or timeout hid the answer. Never re-injected by the ledger.
    DeliveryUnknown,
    /// The worker said it cannot complete the work. A human decides.
    Blocked,
    /// An accepted worker record closed it.
    Done,
    /// Given up. Final.
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Ready => "ready",
            State::Delivering => "delivering",
            State::Delivered => "delivered",
            State::DeliveryUnknown => "delivery_unknown",
            State::Blocked => "blocked",
            State::Done => "done",
            State::Failed => "failed",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(self, State::Done | State::Failed)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a worker record says about the work. It is the worker's own claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Blocked,
    Cannot,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Blocked => "blocked",
            Outcome::Cannot => "cannot",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "done" => Outcome::Done,
            "blocked" => Outcome::Blocked,
            "cannot" => Outcome::Cannot,
            _ => return None,
        })
    }
}

/// Why a worker record did not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// No dispatch has this id.
    UnknownDispatch,
    /// The record comes from a session other than the dispatch's owner.
    WrongSession,
    /// An accepted record already closed this attempt.
    AlreadyClosed,
    /// The record names an attempt older than the open one.
    StaleAttempt,
    /// The record names an attempt that was never delivered.
    FutureAttempt,
    /// The dispatch is not in `delivered`.
    NotDelivered,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::UnknownDispatch => "unknown_dispatch",
            RejectReason::WrongSession => "wrong_session",
            RejectReason::AlreadyClosed => "already_closed",
            RejectReason::StaleAttempt => "stale_attempt",
            RejectReason::FutureAttempt => "future_attempt",
            RejectReason::NotDelivered => "not_delivered",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "unknown_dispatch" => RejectReason::UnknownDispatch,
            "wrong_session" => RejectReason::WrongSession,
            "already_closed" => RejectReason::AlreadyClosed,
            "stale_attempt" => RejectReason::StaleAttempt,
            "future_attempt" => RejectReason::FutureAttempt,
            "not_delivered" => RejectReason::NotDelivered,
            _ => return None,
        })
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One dispatch, as derived from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub id: DispatchId,
    pub content: ContentRef,
    /// The one session this dispatch is delivered to and completed by.
    pub owner: SessionId,
    pub created_by: Code,
    /// Dispatches that must be `done` first.
    pub depends_on: Vec<DispatchId>,
    pub state: State,
    /// The open attempt: the number of deliveries that may have reached the
    /// worker. 0 until the first delivery starts.
    pub attempt: u32,
    /// Redeliveries allowed after the first delivery.
    pub max_retries: u32,
    /// The request id of the latest attempt, if any.
    pub request_id: Option<Code>,
    /// Every request id this dispatch has used. A request id is never reused
    /// by the same dispatch, so an answer to an earlier request cannot be
    /// taken for the answer to a later one.
    pub used_requests: BTreeSet<Code>,
    /// Attempts closed by an accepted record. Each closes once.
    pub closed_attempts: BTreeSet<u32>,
    /// The outcome of the accepted record that moved it to `done` or `blocked`.
    pub outcome: Option<Outcome>,
    /// Why it failed, once it did.
    pub failure: Option<Code>,
    pub created_at: u64,
    pub updated_at: u64,
    pub delivered_at: Option<u64>,
}

impl Dispatch {
    /// Redeliveries used so far.
    pub fn retries(&self) -> u32 {
        self.attempt.saturating_sub(1)
    }
}
