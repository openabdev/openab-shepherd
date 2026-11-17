use super::*;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---- helpers -------------------------------------------------------------

fn id(s: &str) -> DispatchId {
    DispatchId::new(s).unwrap()
}
fn sess(s: &str) -> SessionId {
    SessionId::new(s).unwrap()
}
fn code(s: &str) -> Code {
    Code::new(s).unwrap()
}

/// A clock that ticks one millisecond per call.
fn ticking() -> Clock {
    let n = Arc::new(AtomicU64::new(1_000));
    Box::new(move || n.fetch_add(1, Ordering::SeqCst))
}

#[derive(Default)]
struct Inner {
    bytes: Vec<u8>,
    /// Fail the next append without writing anything.
    fail_write: bool,
    /// Write the record, then report a failure (an fsync that fails).
    fail_sync: bool,
}

/// An in-memory store with fault injection. The handle stays with the test.
#[derive(Clone, Default)]
struct MemStore(Arc<Mutex<Inner>>);

impl MemStore {
    fn with(bytes: &[u8]) -> Self {
        let s = MemStore::default();
        s.0.lock().unwrap().bytes = bytes.to_vec();
        s
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().bytes.clone()
    }
}

impl Store for MemStore {
    fn load(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.bytes())
    }
    fn append(&mut self, record: &[u8]) -> io::Result<()> {
        let mut inner = self.0.lock().unwrap();
        if inner.fail_write {
            return Err(io::Error::other("injected write failure"));
        }
        inner.bytes.extend_from_slice(record);
        if inner.fail_sync {
            return Err(io::Error::other("injected sync failure"));
        }
        Ok(())
    }
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.lock().unwrap().bytes.truncate(len as usize);
        Ok(())
    }
}

fn open_mem(store: &MemStore) -> (Ledger<MemStore>, Recovery) {
    Ledger::open(store.clone(), ticking()).unwrap()
}

fn new_dispatch(name: &str, owner: &str, max_retries: u32, deps: &[&str]) -> NewDispatch {
    NewDispatch {
        id: id(name),
        content: ContentRef::of(&format!("text of {name}")),
        owner: sess(owner),
        created_by: code("operator"),
        depends_on: deps.iter().map(|d| id(d)).collect(),
        max_retries,
    }
}

fn claim(dispatch: &str, attempt: u64, session: &str, outcome: Outcome) -> Claim {
    Claim {
        dispatch_id: dispatch.into(),
        attempt,
        session: session.into(),
        outcome,
        summary: None,
    }
}

/// Answers about the open request: the ledger methods take the request id,
/// and these look it up so a test can say "the runtime answered".
trait Answer {
    fn open_request(&self, id: &DispatchId) -> Code;
    fn confirm(&mut self, id: &DispatchId) -> Result<(), LedgerError>;
    fn refuse(&mut self, id: &DispatchId, reason: Code) -> Result<(), LedgerError>;
    fn unknown(&mut self, id: &DispatchId, reason: Code) -> Result<(), LedgerError>;
    fn not_injected(&mut self, id: &DispatchId) -> Result<(), LedgerError>;
}

impl<S: Store> Answer for Ledger<S> {
    fn open_request(&self, id: &DispatchId) -> Code {
        self.get(id)
            .and_then(|d| d.request_id.clone())
            .unwrap_or_else(|| code("none"))
    }
    fn confirm(&mut self, id: &DispatchId) -> Result<(), LedgerError> {
        let r = self.open_request(id);
        self.confirm_delivered(id, &r)
    }
    fn refuse(&mut self, id: &DispatchId, reason: Code) -> Result<(), LedgerError> {
        let r = self.open_request(id);
        self.refuse_delivery(id, &r, reason)
    }
    fn unknown(&mut self, id: &DispatchId, reason: Code) -> Result<(), LedgerError> {
        let r = self.open_request(id);
        self.mark_delivery_unknown(id, &r, reason)
    }
    fn not_injected(&mut self, id: &DispatchId) -> Result<(), LedgerError> {
        let r = self.open_request(id);
        self.resolve_not_injected(id, &r)
    }
}

fn state_of(l: &Ledger<MemStore>, name: &str) -> State {
    l.get(&id(name)).unwrap().state
}

fn deliver(l: &mut Ledger<MemStore>, name: &str) {
    let next = l.get(&id(name)).unwrap().attempt + 1;
    let n = l
        .begin_delivery(&id(name), code(&format!("req{next}")))
        .unwrap();
    assert_eq!(n, next);
    l.confirm(&id(name)).unwrap();
}

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ledger-test-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

fn count_type(bytes: &[u8], kind: &str) -> usize {
    lines(bytes).iter().filter(|v| v["type"] == kind).count()
}

// ---- names and content reference ------------------------------------------

#[test]
fn names_are_validated() {
    assert_eq!(DispatchId::new(""), Err(NameError::Empty));
    assert_eq!(DispatchId::new("a".repeat(129)), Err(NameError::TooLong));
    assert!(DispatchId::new("a".repeat(128)).is_ok());
    for bad in ["a b", "a\nb", "a\"b", "a/b", "é", "a,b"] {
        assert_eq!(SessionId::new(bad), Err(NameError::BadChar), "{bad:?}");
    }
    assert!(Code::new("not_idle").is_ok());
    assert!(DispatchId::new("01J0:abc-1.x_y").is_ok());
}

#[test]
fn content_ref_is_a_hash_and_a_length() {
    let c = ContentRef::of("abc");
    assert_eq!(
        c.sha256(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(c.len(), 3);
    assert!(ContentRef::of("").is_empty());
    assert!(ContentRef::new("ABC", 1).is_err());
    assert!(ContentRef::new("0".repeat(63), 1).is_err());
    assert!(ContentRef::new("0".repeat(64), 1).is_ok());
}

#[test]
fn the_log_never_holds_the_content_text() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    let mut n = new_dispatch("a", "s1", 1, &[]);
    n.content = ContentRef::of("hunter2-secret-text");
    l.create(n).unwrap();
    deliver(&mut l, "a");
    let mut c = claim("a", 1, "s1", Outcome::Done);
    c.summary = Some(ContentRef::of("secret summary words"));
    l.submit_record(c).unwrap();
    let log = String::from_utf8(store.bytes()).unwrap();
    assert!(!log.contains("hunter2"));
    assert!(!log.contains("secret summary"));
    assert!(log.contains(ContentRef::of("hunter2-secret-text").sha256()));
    assert!(log.contains("summary_sha256"));
}

// ---- state machine ---------------------------------------------------------

#[test]
fn happy_path_and_dependents() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    l.create(new_dispatch("b", "s1", 1, &["a"])).unwrap();
    assert_eq!(state_of(&l, "a"), State::Ready);
    assert_eq!(state_of(&l, "b"), State::Pending);

    assert_eq!(l.begin_delivery(&id("a"), code("r1")).unwrap(), 1);
    assert_eq!(state_of(&l, "a"), State::Delivering);
    l.confirm(&id("a")).unwrap();
    assert_eq!(state_of(&l, "a"), State::Delivered);
    assert!(l.get(&id("a")).unwrap().delivered_at.is_some());

    let out = l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap();
    assert_eq!(out, RecordOutcome::Accepted(Outcome::Done));
    assert_eq!(state_of(&l, "a"), State::Done);
    assert_eq!(
        state_of(&l, "b"),
        State::Ready,
        "done releases its dependents"
    );
}

#[test]
fn a_dependency_that_is_done_at_creation_makes_it_ready() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    deliver(&mut l, "a");
    l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap();
    l.create(new_dispatch("b", "s1", 0, &["a"])).unwrap();
    assert_eq!(state_of(&l, "b"), State::Ready);
}

#[test]
fn dependency_errors_are_refused_at_creation_and_write_nothing() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    let before = store.bytes();
    let cases = [
        (new_dispatch("x", "s1", 0, &["x"]), "self"),
        (new_dispatch("x", "s1", 0, &["nope"]), "unknown"),
        (new_dispatch("x", "s1", 0, &["a", "a"]), "twice"),
        (new_dispatch("a", "s1", 0, &[]), "duplicate id"),
    ];
    for (n, what) in cases {
        assert!(
            matches!(l.create(n), Err(LedgerError::Transition(_))),
            "{what}"
        );
    }
    assert_eq!(store.bytes(), before);
    // Dependencies must exist already, so a cycle has no way to be written.
    assert!(matches!(
        l.create(new_dispatch("a2", "s1", 0, &["a3"])),
        Err(LedgerError::Transition(TransitionError::UnknownDependency(
            _
        )))
    ));
}

#[test]
fn refused_delivery_returns_to_ready_and_the_attempt_stays_used() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 3, &[])).unwrap();
    for n in 1..=4u32 {
        assert_eq!(l.begin_delivery(&id("a"), code("r")).unwrap(), n);
        l.refuse(&id("a"), code("not_idle")).unwrap();
        assert_eq!(state_of(&l, "a"), State::Ready);
        assert_eq!(l.get(&id("a")).unwrap().attempt, n);
    }
    // The fifth request would exceed max_retries + 1: the dispatch fails,
    // visibly, and the call says why.
    assert!(matches!(
        l.begin_delivery(&id("a"), code("r")),
        Err(LedgerError::Transition(TransitionError::RetryCapReached(_)))
    ));
    assert_eq!(state_of(&l, "a"), State::Failed);
}

#[test]
fn retry_cap_is_enforced_and_never_silent() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap(); // one retry: two attempts
    deliver(&mut l, "a");
    l.request_retry(&id("a")).unwrap();
    assert_eq!(state_of(&l, "a"), State::Ready);
    deliver(&mut l, "a");
    assert_eq!(l.get(&id("a")).unwrap().attempt, 2);
    assert_eq!(l.get(&id("a")).unwrap().retries(), 1);
    assert!(matches!(
        l.request_retry(&id("a")),
        Err(LedgerError::Transition(TransitionError::RetryCapReached(_)))
    ));
    // Reaching the cap is not a silent stop: the dispatch is failed.
    assert_eq!(state_of(&l, "a"), State::Failed);
    assert_eq!(
        l.get(&id("a")).unwrap().failure,
        Some(code("retry_cap_reached"))
    );
    // Final: nothing moves a failed dispatch.
    assert!(l.begin_delivery(&id("a"), code("r")).is_err());
    assert!(l.fail(&id("a"), code("again")).is_err());
}

#[test]
fn a_dispatch_at_the_cap_fails_when_asked_to_deliver_again() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    l.begin_delivery(&id("a"), code("r1")).unwrap();
    l.refuse(&id("a"), code("not_idle")).unwrap();
    assert!(matches!(
        l.begin_delivery(&id("a"), code("r2")),
        Err(LedgerError::Transition(TransitionError::RetryCapReached(_)))
    ));
    assert_eq!(state_of(&l, "a"), State::Failed);
}

#[test]
fn zero_retries_means_one_attempt() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    deliver(&mut l, "a");
    assert!(matches!(
        l.request_retry(&id("a")),
        Err(LedgerError::Transition(TransitionError::RetryCapReached(_)))
    ));
}

#[test]
fn delivery_unknown_leaves_only_by_runtime_answer() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    l.begin_delivery(&id("a"), code("r")).unwrap();
    l.unknown(&id("a"), code("timeout")).unwrap();
    assert_eq!(state_of(&l, "a"), State::DeliveryUnknown);
    // Not by a retry, a second delivery, or a worker record.
    assert!(l.request_retry(&id("a")).is_err());
    assert!(l.begin_delivery(&id("a"), code("r2")).is_err());
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::NotDelivered)
    );
    // The runtime's audit shows no injection: back to ready, attempt returned.
    l.not_injected(&id("a")).unwrap();
    assert_eq!(state_of(&l, "a"), State::Ready);
    assert_eq!(l.get(&id("a")).unwrap().attempt, 1);

    // Or the audit shows it was injected.
    assert_eq!(l.begin_delivery(&id("a"), code("r3")).unwrap(), 2);
    l.unknown(&id("a"), code("timeout")).unwrap();
    l.confirm(&id("a")).unwrap();
    assert_eq!(state_of(&l, "a"), State::Delivered);
}

/// The failing order from review: request r1 is lost and found not injected,
/// request r2 follows, and r1's answer arrives late.
#[test]
fn a_late_answer_to_an_earlier_request_cannot_move_a_later_one() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 3, &[])).unwrap();
    let (r1, r2) = (code("r1"), code("r2"));
    assert_eq!(l.begin_delivery(&id("a"), r1.clone()).unwrap(), 1);
    l.mark_delivery_unknown(&id("a"), &r1, code("timeout"))
        .unwrap();
    l.resolve_not_injected(&id("a"), &r1).unwrap();
    // The next request gets a new attempt number, never the same one.
    assert_eq!(l.begin_delivery(&id("a"), r2.clone()).unwrap(), 2);

    let before = l.state().clone();
    let log_before = count_type(&l.store.bytes(), "delivered");
    // Late answers naming r1, in every form.
    assert!(matches!(
        l.confirm_delivered(&id("a"), &r1),
        Err(LedgerError::Transition(
            TransitionError::WrongRequest { .. }
        ))
    ));
    assert!(l.refuse_delivery(&id("a"), &r1, code("late")).is_err());
    assert!(l
        .mark_delivery_unknown(&id("a"), &r1, code("late"))
        .is_err());
    assert!(l.resolve_not_injected(&id("a"), &r1).is_err());
    assert_eq!(l.state(), &before);
    assert_eq!(count_type(&l.store.bytes(), "delivered"), log_before);
    assert_eq!(state_of(&l, "a"), State::Delivering);

    // A late worker record for the first generation is stale, and so is one
    // that cannot yet be matched.
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::StaleAttempt)
    );
    assert_eq!(state_of(&l, "a"), State::Delivering);

    // r2 itself still works, after which the r1 record stays stale and the
    // r2 record closes it.
    l.confirm_delivered(&id("a"), &r2).unwrap();
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::StaleAttempt)
    );
    assert_eq!(state_of(&l, "a"), State::Delivered);
    assert_eq!(
        l.submit_record(claim("a", 2, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Accepted(Outcome::Done)
    );
    assert_eq!(state_of(&l, "a"), State::Done);
}

#[test]
fn a_late_answer_after_a_refusal_cannot_move_the_dispatch() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 3, &[])).unwrap();
    let r1 = code("r1");
    l.begin_delivery(&id("a"), r1.clone()).unwrap();
    l.refuse_delivery(&id("a"), &r1, code("not_idle")).unwrap();
    assert!(l.confirm_delivered(&id("a"), &r1).is_err());
    assert_eq!(state_of(&l, "a"), State::Ready);
    l.begin_delivery(&id("a"), code("r2")).unwrap();
    assert!(l.confirm_delivered(&id("a"), &r1).is_err());
    assert_eq!(state_of(&l, "a"), State::Delivering);
}

#[test]
fn blocked_record_blocks_and_unblock_reopens_with_the_attempt_closed() {
    let (mut l, _) = open_mem(&MemStore::default());
    l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
    deliver(&mut l, "a");
    let out = l
        .submit_record(claim("a", 1, "s1", Outcome::Cannot))
        .unwrap();
    assert_eq!(out, RecordOutcome::Accepted(Outcome::Cannot));
    assert_eq!(state_of(&l, "a"), State::Blocked);
    l.unblock(&id("a")).unwrap();
    assert_eq!(state_of(&l, "a"), State::Ready);
    // A late repeat of the closed attempt does nothing.
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::AlreadyClosed)
    );
    assert_eq!(state_of(&l, "a"), State::Ready);
    deliver(&mut l, "a");
    // Attempt 1 is stale now; attempt 2 counts.
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::AlreadyClosed)
    );
    assert_eq!(
        l.submit_record(claim("a", 2, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Accepted(Outcome::Done)
    );
}

// ---- the matching rule -----------------------------------------------------

/// Build dispatch `a` (owner s1) in a named situation. Returns the attempt a
/// record is allowed to name, if any record may be accepted at all.
fn build_a(l: &mut Ledger<MemStore>, scenario: &str) -> Option<u64> {
    let a = id("a");
    match scenario {
        "pending" => {
            l.create(new_dispatch("gate", "s9", 0, &[])).unwrap();
            l.create(new_dispatch("a", "s1", 2, &["gate"])).unwrap();
            None
        }
        "ready" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            None
        }
        "delivering" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            l.begin_delivery(&a, code("r")).unwrap();
            None
        }
        "delivered1" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            deliver(l, "a");
            Some(1)
        }
        "delivery_unknown" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            l.begin_delivery(&a, code("r")).unwrap();
            l.unknown(&a, code("timeout")).unwrap();
            None
        }
        "delivered2" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            deliver(l, "a");
            l.request_retry(&a).unwrap();
            deliver(l, "a");
            Some(2)
        }
        "ready_after_retry" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            deliver(l, "a");
            l.request_retry(&a).unwrap();
            None
        }
        "blocked" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            deliver(l, "a");
            l.submit_record(claim("a", 1, "s1", Outcome::Blocked))
                .unwrap();
            None
        }
        "done" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            deliver(l, "a");
            l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap();
            None
        }
        "failed" => {
            l.create(new_dispatch("a", "s1", 2, &[])).unwrap();
            l.fail(&a, code("gave_up")).unwrap();
            None
        }
        other => panic!("no scenario {other}"),
    }
}

const SCENARIOS: [&str; 10] = [
    "pending",
    "ready",
    "delivering",
    "delivered1",
    "delivery_unknown",
    "delivered2",
    "ready_after_retry",
    "blocked",
    "done",
    "failed",
];

#[test]
fn only_a_record_matching_every_part_moves_a_dispatch_to_done() {
    let mut accepted_cases = 0;
    for scenario in SCENARIOS {
        for target in ["a", "b", "nope"] {
            for attempt in 0..=4u64 {
                for who in ["s1", "s2", "s3"] {
                    let store = MemStore::default();
                    let (mut l, _) = open_mem(&store);
                    let a_accepts = build_a(&mut l, scenario);
                    // `b` belongs to s2 and sits delivered, attempt 1.
                    l.create(new_dispatch("b", "s2", 2, &[])).unwrap();
                    deliver(&mut l, "b");
                    let before = l.state().clone();
                    let log_before = lines(&store.bytes()).len();

                    // Written out by hand, not read from the ledger.
                    let expect_accept =
                        (target == "a" && a_accepts == Some(attempt) && who == "s1")
                            || (target == "b" && attempt == 1 && who == "s2");

                    let got = l
                        .submit_record(claim(target, attempt, who, Outcome::Done))
                        .unwrap();
                    let case = format!("{scenario} {target} attempt {attempt} from {who}");
                    assert_eq!(
                        matches!(got, RecordOutcome::Accepted(_)),
                        expect_accept,
                        "{case}: {got:?}"
                    );
                    assert_eq!(
                        lines(&store.bytes()).len(),
                        log_before + 1,
                        "{case}: one event"
                    );
                    if expect_accept {
                        accepted_cases += 1;
                        assert_eq!(state_of(&l, target), State::Done, "{case}");
                    } else {
                        // Rejected: recorded, and nothing else moved.
                        assert_eq!(l.state(), &before, "{case}");
                        assert_eq!(count_type(&store.bytes(), "record_rejected"), 1, "{case}");
                    }
                    // Done never appears by any other route.
                    for d in l.state().iter() {
                        if d.state == State::Done
                            && before.get(&d.id).map(|d| d.state) != Some(State::Done)
                        {
                            assert!(expect_accept && d.id.as_str() == target, "{case}");
                        }
                    }
                }
            }
        }
    }
    // delivered1 and delivered2 accept for `a`; every scenario accepts for `b`.
    assert_eq!(accepted_cases, 2 + SCENARIOS.len());
}

#[test]
fn rejection_reasons_are_typed() {
    let mut cases: Vec<(&str, &str, u64, &str, RejectReason)> = vec![
        ("delivered2", "nope", 1, "s1", RejectReason::UnknownDispatch),
        ("delivered2", "a", 2, "s3", RejectReason::WrongSession),
        ("delivered2", "a", 1, "s1", RejectReason::StaleAttempt),
        ("delivered2", "a", 3, "s1", RejectReason::FutureAttempt),
        ("delivered2", "a", 0, "s1", RejectReason::StaleAttempt),
        ("delivered1", "a", 7, "s1", RejectReason::FutureAttempt),
        ("ready", "a", 1, "s1", RejectReason::FutureAttempt),
        ("ready", "a", 0, "s1", RejectReason::NotDelivered),
        ("pending", "a", 0, "s1", RejectReason::NotDelivered),
        ("delivering", "a", 1, "s1", RejectReason::NotDelivered),
        ("delivery_unknown", "a", 1, "s1", RejectReason::NotDelivered),
        (
            "ready_after_retry",
            "a",
            1,
            "s1",
            RejectReason::NotDelivered,
        ),
        ("failed", "a", 0, "s1", RejectReason::NotDelivered),
        ("done", "a", 1, "s1", RejectReason::AlreadyClosed),
        ("blocked", "a", 1, "s1", RejectReason::AlreadyClosed),
    ];
    // A name the ledger would not accept is just an unknown dispatch.
    cases.push(("ready", "a b\n", 1, "s1", RejectReason::UnknownDispatch));
    for (scenario, target, attempt, who, reason) in cases {
        let (mut l, _) = open_mem(&MemStore::default());
        build_a(&mut l, scenario);
        let got = l
            .submit_record(claim(target, attempt, who, Outcome::Done))
            .unwrap();
        assert_eq!(
            got,
            RecordOutcome::Rejected(reason),
            "{scenario} {target} {attempt} {who}"
        );
    }
}

#[test]
fn a_second_record_for_the_same_attempt_is_rejected() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    deliver(&mut l, "a");
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Accepted(Outcome::Done)
    );
    for _ in 0..3 {
        assert_eq!(
            l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
            RecordOutcome::Rejected(RejectReason::AlreadyClosed)
        );
    }
    assert_eq!(count_type(&store.bytes(), "record_accepted"), 1);
    assert_eq!(count_type(&store.bytes(), "record_rejected"), 3);
}

#[test]
fn a_record_before_the_dispatch_exists_does_not_satisfy_it_later() {
    let (mut l, _) = open_mem(&MemStore::default());
    assert_eq!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)).unwrap(),
        RecordOutcome::Rejected(RejectReason::UnknownDispatch)
    );
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    deliver(&mut l, "a");
    assert_eq!(
        state_of(&l, "a"),
        State::Delivered,
        "the earlier record left no mark"
    );
}

#[test]
fn a_rejected_record_stores_only_capped_claims() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    let long = "x".repeat(10_000);
    let out = l
        .submit_record(claim(&long, 1, &long, Outcome::Done))
        .unwrap();
    assert_eq!(out, RecordOutcome::Rejected(RejectReason::UnknownDispatch));
    let log = store.bytes();
    assert!(log.len() < 1_000);
    assert!(replay(&log).is_ok());
}

// ---- replay ----------------------------------------------------------------

/// A longer scripted history that touches every event kind.
fn script(l: &mut Ledger<MemStore>) {
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    l.create(new_dispatch("b", "s1", 1, &["a"])).unwrap();
    l.create(new_dispatch("c", "s2", 1, &[])).unwrap();
    deliver(l, "a");
    l.request_retry(&id("a")).unwrap();
    l.begin_delivery(&id("a"), code("r2")).unwrap();
    l.confirm(&id("a")).unwrap();
    l.submit_record(claim("a", 2, "s3", Outcome::Done)).unwrap(); // rejected
    l.submit_record(claim("a", 2, "s1", Outcome::Done)).unwrap(); // accepted, releases b
    l.begin_delivery(&id("b"), code("r3")).unwrap();
    l.unknown(&id("b"), code("timeout")).unwrap();
    l.not_injected(&id("b")).unwrap();
    l.begin_delivery(&id("b"), code("r4")).unwrap();
    l.confirm(&id("b")).unwrap();
    l.begin_delivery(&id("c"), code("r5")).unwrap();
    l.refuse(&id("c"), code("not_idle")).unwrap();
    deliver(l, "c");
    l.submit_record(claim("c", 2, "s2", Outcome::Blocked))
        .unwrap();
    l.unblock(&id("c")).unwrap();
    l.fail(&id("c"), code("operator_gave_up")).unwrap();
    l.begin_delivery(&id("b"), code("r6")).unwrap_err(); // delivered: refused, not logged
}

#[test]
fn replay_equals_live_state_after_the_scripted_operations() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    script(&mut l);
    let r = replay(&store.bytes()).unwrap();
    assert_eq!(&r.state, l.state());
    assert_eq!(r.torn_bytes, 0);
    assert_eq!(r.records, lines(&store.bytes()).len() as u64);
    // Every event kind was exercised.
    for kind in [
        "created",
        "delivery_started",
        "delivered",
        "delivery_refused",
        "delivery_unknown",
        "delivery_not_injected",
        "retry_requested",
        "record_accepted",
        "record_rejected",
        "unblocked",
        "failed",
    ] {
        assert!(count_type(&store.bytes(), kind) > 0, "{kind}");
    }
}

#[test]
fn replay_is_idempotent() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    script(&mut l);
    let bytes = store.bytes();
    let first = replay(&bytes).unwrap();
    assert_eq!(first, replay(&bytes).unwrap());

    // Opening twice changes nothing: no new events, same state.
    let (l1, rec1) = Ledger::open(MemStore::with(&bytes), ticking()).unwrap();
    assert!(rec1.moved_to_delivery_unknown.is_empty());
    let store2 = MemStore::with(&bytes);
    let (l2, rec2) = Ledger::open(store2.clone(), ticking()).unwrap();
    assert_eq!(rec1, rec2);
    assert_eq!(l1.state(), l2.state());
    assert_eq!(store2.bytes(), bytes);
}

/// A tiny deterministic generator, so the random walk is the same every run.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

#[test]
fn random_walks_keep_replay_equal_and_done_honest() {
    let names = ["d0", "d1", "d2", "d3", "d4"];
    let sessions = ["s1", "s2", "s3"];
    let mut accepted_total = 0;
    let mut rejected_total = 0;
    for seed in 0..90u64 {
        let mut rng = Lcg(seed + 1);
        let store = MemStore::default();
        let (mut l, _) = open_mem(&store);
        for step in 0..120 {
            let name = names[rng.next(5) as usize];
            let d = id(name);
            let ctx = format!("seed {seed} step {step}");
            match rng.next(11) {
                0 => {
                    let owner = sessions[rng.next(3) as usize];
                    let dep = names[rng.next(5) as usize];
                    let deps: &[&str] = if rng.next(2) == 0 { &[] } else { &[dep] };
                    let _ = l.create(new_dispatch(name, owner, 1 + rng.next(3) as u32, deps));
                }
                1 => {
                    let _ = l.begin_delivery(&d, code("r"));
                }
                2 => {
                    let _ = l.confirm(&d);
                }
                3 => {
                    let _ = l.refuse(&d, code("refused"));
                }
                4 => {
                    let _ = l.unknown(&d, code("timeout"));
                }
                5 => {
                    let _ = l.not_injected(&d);
                }
                6 => {
                    let _ = l.request_retry(&d);
                }
                7 => {
                    let _ = l.unblock(&d);
                }
                8 => {
                    if rng.next(4) == 0 {
                        let _ = l.fail(&d, code("gave_up"));
                    }
                }
                _ => {
                    let pre = l.get(&d).cloned();
                    // Mostly aim at the real owner and open attempt, so the
                    // walk reaches acceptance; sometimes aim anywhere.
                    let (mut who, mut attempt) = (sessions[rng.next(3) as usize], rng.next(4));
                    if let Some(p) = pre.as_ref().filter(|_| rng.next(4) > 0) {
                        if rng.next(3) > 0 {
                            who = sessions.iter().find(|s| **s == p.owner.as_str()).unwrap();
                        }
                        if rng.next(3) > 0 {
                            attempt = u64::from(p.attempt);
                        }
                    }
                    let outcome =
                        [Outcome::Done, Outcome::Done, Outcome::Blocked][rng.next(3) as usize];
                    let out = l.submit_record(claim(name, attempt, who, outcome)).unwrap();
                    let should = pre.as_ref().is_some_and(|p| {
                        p.state == State::Delivered
                            && u64::from(p.attempt) == attempt
                            && p.owner.as_str() == who
                            && !p.closed_attempts.contains(&(attempt as u32))
                    });
                    assert_eq!(matches!(out, RecordOutcome::Accepted(_)), should, "{ctx}");
                    if should {
                        accepted_total += 1;
                    } else {
                        rejected_total += 1;
                    }
                }
            }
            // Replay of the bytes on disk equals the live state, every step.
            let r = replay(&store.bytes()).unwrap();
            assert_eq!(&r.state, l.state(), "{ctx}");
        }
        // A dispatch is done only if an accepted `done` record is in the log.
        let log = lines(&store.bytes());
        for dsp in l.state().iter().filter(|d| d.state == State::Done) {
            assert!(
                log.iter().any(|e| e["type"] == "record_accepted"
                    && e["outcome"] == "done"
                    && e["id"] == dsp.id.as_str()),
                "seed {seed}: {} is done without a record",
                dsp.id
            );
        }
    }
    assert!(
        accepted_total > 20,
        "the walk should accept some records: {accepted_total}"
    );
    assert!(rejected_total > 100, "and reject many: {rejected_total}");
}

// ---- log format, torn tail, corruption --------------------------------------

#[test]
fn golden_record_pins_the_format() {
    let ev = Event::Created {
        id: id("d1"),
        content: ContentRef::of("abc"),
        owner: sess("s1"),
        created_by: code("operator"),
        depends_on: vec![],
        max_retries: 2,
        at: 5,
    };
    assert_eq!(
        String::from_utf8(encode_record(1, &ev)).unwrap(),
        "{\"at\":5,\"content_len\":3,\"content_sha256\":\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\",\"created_by\":\"operator\",\"depends_on\":[],\"id\":\"d1\",\"max_retries\":2,\"owner\":\"s1\",\"seq\":1,\"type\":\"created\",\"v\":1}\n"
    );
}

#[test]
fn a_torn_final_record_is_cut_off_and_appending_continues_cleanly() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    deliver(&mut l, "a");
    let good = store.bytes();
    let next = encode_record(
        99,
        &Event::Created {
            id: id("zz"),
            content: ContentRef::of("x"),
            owner: sess("s1"),
            created_by: code("operator"),
            depends_on: vec![],
            max_retries: 0,
            at: 1,
        },
    );
    // Every proper prefix of the next record, and the whole of it minus its newline.
    for cut in 1..next.len() {
        let mut bytes = good.clone();
        bytes.extend_from_slice(&next[..cut]);
        let r = replay(&bytes).unwrap();
        assert_eq!(r.torn_bytes, cut as u64, "cut {cut}");
        assert_eq!(r.valid_len, good.len() as u64);

        let s = MemStore::with(&bytes);
        let (mut l2, rec) = Ledger::open(s.clone(), ticking()).unwrap();
        assert_eq!(rec.torn_bytes, cut as u64);
        assert_eq!(s.bytes(), good, "the torn tail is gone");
        l2.create(new_dispatch("b", "s1", 0, &[])).unwrap();
        let again = replay(&s.bytes()).unwrap();
        assert_eq!(again.torn_bytes, 0);
        assert_eq!(&again.state, l2.state());
    }
}

#[test]
fn a_complete_looking_record_without_its_newline_is_torn() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    let mut bytes = store.bytes();
    bytes.pop(); // lose only the newline
    let r = replay(&bytes).unwrap();
    assert_eq!(r.records, 0);
    assert!(r.state.is_empty());
}

#[test]
fn corruption_is_an_error_not_a_guess() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    deliver(&mut l, "a");
    let good = String::from_utf8(store.bytes()).unwrap();
    let ls: Vec<&str> = good.lines().collect();
    assert_eq!(ls.len(), 3);

    let join = |parts: &[&str]| -> Vec<u8> {
        let mut s = parts.join("\n");
        s.push('\n');
        s.into_bytes()
    };
    // Garbage in the middle.
    let e = replay(&join(&[ls[0], "{not json", ls[2]])).unwrap_err();
    assert_eq!(e.line, 2);
    // A missing record: the sequence skips.
    let e = replay(&join(&[ls[0], ls[2]])).unwrap_err();
    assert_eq!(e.line, 2);
    assert!(e.reason.contains("sequence"));
    // A complete last line that is not valid is corruption, not a torn tail.
    assert!(replay(&join(&[ls[0], ls[1], "{\"v\":1"])).is_err());
    // A version we do not know.
    assert!(replay(&join(&[&ls[0].replace("\"v\":1", "\"v\":2")])).is_err());
    // An event type we do not know.
    assert!(replay(&join(&[&ls[0].replace("\"created\"", "\"teleported\"")])).is_err());
    // A field that breaks a rule.
    assert!(replay(&join(&[&ls[0]
        .replace("\"d", "\"d d")
        .replace("\"a\"", "\"a b\"")]))
    .is_err());
    // The ledger refuses to open over it.
    let s = MemStore::with(&join(&[ls[0], "{not json"]));
    assert!(matches!(
        Ledger::open(s, ticking()),
        Err(LedgerError::Replay(_))
    ));
}

#[test]
fn a_forged_done_that_breaks_the_matching_rule_does_not_replay() {
    // Only records the rule would reject are refused on replay. A hand-written
    // record that names the right id, session and open attempt replays: the
    // log is accountability, not tamper-proofing (docs/ledger.md).
    let mut log = Vec::new();
    let created = |n: &str| Event::Created {
        id: id(n),
        content: ContentRef::of("x"),
        owner: sess("s1"),
        created_by: code("operator"),
        depends_on: vec![],
        max_retries: 0,
        at: 1,
    };
    log.extend(encode_record(1, &created("a")));
    // Accepted while the dispatch is only `ready`.
    let forged = |attempt: u32, session: &str| Event::RecordAccepted {
        id: id("a"),
        attempt,
        session: sess(session),
        outcome: Outcome::Done,
        summary: None,
        at: 2,
    };
    let mut forged_log = log.clone();
    forged_log.extend(encode_record(2, &forged(1, "s1")));
    let e = replay(&forged_log).unwrap_err();
    assert_eq!(e.line, 2);

    // And after delivery, still not for another session or another attempt.
    for (attempt, session) in [(1, "s2"), (2, "s1"), (0, "s1")] {
        let mut l = log.clone();
        l.extend(encode_record(
            2,
            &Event::DeliveryStarted {
                id: id("a"),
                attempt: 1,
                request_id: code("r"),
                at: 2,
            },
        ));
        l.extend(encode_record(
            3,
            &Event::Delivered {
                id: id("a"),
                attempt: 1,
                request_id: code("r"),
                at: 3,
            },
        ));
        l.extend(encode_record(4, &forged(attempt, session)));
        assert!(replay(&l).is_err(), "attempt {attempt} from {session}");
    }
}

// ---- crash recovery --------------------------------------------------------

fn done_ids_in(bytes: &[u8]) -> Vec<String> {
    // Only complete lines count as records.
    let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut out: Vec<String> = lines(&bytes[..end])
        .iter()
        .filter(|e| e["type"] == "record_accepted" && e["outcome"] == "done")
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

/// Open over `prefix` and check everything a restart must guarantee.
fn check_restart(prefix: &[u8], store: impl Store, read_back: impl Fn() -> Vec<u8>) {
    let before = replay(prefix).unwrap();
    let was_delivering = before.state.delivering();
    let (l, rec) = Ledger::open(store, ticking()).unwrap();

    // Every dispatch that was `delivering` is now `delivery_unknown`, with the
    // same attempt, and none was moved anywhere else.
    assert_eq!(
        rec.moved_to_delivery_unknown,
        was_delivering
            .iter()
            .map(|(i, _, _)| i.clone())
            .collect::<Vec<_>>()
    );
    for (i, attempt, _) in &was_delivering {
        let d = l.get(i).unwrap();
        assert_eq!(d.state, State::DeliveryUnknown);
        assert_eq!(d.attempt, *attempt);
    }
    assert!(l.state().delivering().is_empty());
    // No other dispatch changed state.
    for d in before.state.iter().filter(|d| d.state != State::Delivering) {
        assert_eq!(l.get(&d.id).unwrap(), d);
    }
    // `done` only ever comes from a complete accepted record in the prefix.
    let mut done: Vec<String> = l
        .state()
        .iter()
        .filter(|d| d.state == State::Done)
        .map(|d| d.id.to_string())
        .collect();
    done.sort();
    assert_eq!(done, done_ids_in(prefix));

    // What is on disk now is a clean log that replays to the same state.
    let bytes = read_back();
    let after = replay(&bytes).unwrap();
    assert_eq!(after.torn_bytes, 0);
    assert_eq!(&after.state, l.state());
    drop(l);

    // A second restart finds nothing left to repair.
    let s2 = MemStore::with(&bytes);
    let (l2, rec2) = Ledger::open(s2.clone(), ticking()).unwrap();
    assert!(rec2.moved_to_delivery_unknown.is_empty());
    assert_eq!(rec2.torn_bytes, 0);
    assert_eq!(l2.state(), &after.state);
    assert_eq!(s2.bytes(), bytes);
}

fn crash_script_log() -> Vec<u8> {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    script(&mut l);
    // End with a dispatch caught mid-delivery.
    l.create(new_dispatch("e", "s4", 1, &[])).unwrap();
    l.begin_delivery(&id("e"), code("r9")).unwrap();
    store.bytes()
}

#[test]
fn a_crash_at_every_byte_of_the_log_recovers_to_delivery_unknown() {
    let log = crash_script_log();
    assert!(log.len() > 2_000);
    for n in 0..=log.len() {
        let prefix = &log[..n];
        let store = MemStore::with(prefix);
        check_restart(prefix, store.clone(), || store.bytes());
    }
}

#[test]
fn a_crash_between_delivering_and_delivered_is_delivery_unknown() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 3, &[])).unwrap();
    l.begin_delivery(&id("a"), code("r")).unwrap();
    let crashed = store.bytes(); // the process dies here

    let s = MemStore::with(&crashed);
    let (mut l2, rec) = Ledger::open(s.clone(), ticking()).unwrap();
    assert_eq!(rec.moved_to_delivery_unknown, vec![id("a")]);
    assert_eq!(state_of(&l2, "a"), State::DeliveryUnknown);
    assert_eq!(count_type(&s.bytes(), "delivery_unknown"), 1);
    // Not delivered again, not retried, not done, whatever the caller tries.
    assert!(l2.begin_delivery(&id("a"), code("r2")).is_err());
    assert!(l2.request_retry(&id("a")).is_err());
    assert_eq!(
        l2.submit_record(claim("a", 1, "s1", Outcome::Done))
            .unwrap(),
        RecordOutcome::Rejected(RejectReason::NotDelivered)
    );
    assert_eq!(state_of(&l2, "a"), State::DeliveryUnknown);
}

#[test]
fn crash_recovery_on_a_real_file_at_record_boundaries_and_inside_records() {
    let log = crash_script_log();
    let mut cuts: Vec<usize> = vec![0, log.len()];
    let mut start = 0;
    for (i, b) in log.iter().enumerate() {
        if *b == b'\n' {
            let end = i + 1;
            cuts.extend([end, start + 1, start + (end - start) / 2, end - 1]);
            start = end;
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let dir = TmpDir::new("crash");
    for (i, n) in cuts.into_iter().enumerate() {
        let path = dir.file(&format!("log-{i}"));
        fs::write(&path, &log[..n]).unwrap();
        let read = path.clone();
        check_restart(&log[..n], FileStore::open(&path).unwrap(), move || {
            fs::read(&read).unwrap()
        });
    }
}

// ---- failing storage -------------------------------------------------------

fn snapshot(l: &Ledger<MemStore>) -> (LedgerState, bool) {
    (l.state().clone(), l.is_failed())
}

#[test]
fn a_failed_write_refuses_the_change_and_leaves_state_unchanged() {
    for sync_fails in [false, true] {
        let store = MemStore::default();
        let (mut l, _) = open_mem(&store);
        l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
        let (state, _) = snapshot(&l);
        {
            let mut inner = store.0.lock().unwrap();
            inner.fail_write = !sync_fails;
            inner.fail_sync = sync_fails;
        }

        // A delivery is refused before anything is injected.
        let err = l.begin_delivery(&id("a"), code("r")).unwrap_err();
        assert!(matches!(err, LedgerError::Store(_)), "{err}");
        assert_eq!(l.state(), &state);
        assert_eq!(state_of(&l, "a"), State::Ready);
        assert!(l.is_failed());

        // Faults gone, the ledger still refuses: the file may hold a record
        // the ledger did not apply, so only a reopen is safe.
        {
            let mut inner = store.0.lock().unwrap();
            inner.fail_write = false;
            inner.fail_sync = false;
        }
        assert!(matches!(
            l.create(new_dispatch("b", "s1", 0, &[])),
            Err(LedgerError::LogUnavailable)
        ));
        assert!(matches!(
            l.submit_record(claim("a", 1, "s1", Outcome::Done)),
            Err(LedgerError::LogUnavailable)
        ));
        assert_eq!(l.state(), &state);
        // Reads keep working.
        assert_eq!(l.get(&id("a")).unwrap().state, State::Ready);

        // Reopening replays whatever is durable and works again.
        let (mut l2, _) = open_mem(&store);
        assert!(l2.get(&id("a")).is_some());
        l2.create(new_dispatch("b", "s1", 0, &[])).unwrap();
    }
}

#[test]
fn a_record_whose_log_write_fails_is_an_error_not_a_silent_drop() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    deliver(&mut l, "a");
    let (state, _) = snapshot(&l);
    store.0.lock().unwrap().fail_write = true;
    // Both an acceptable and a rejectable record surface the failure.
    assert!(matches!(
        l.submit_record(claim("a", 1, "s1", Outcome::Done)),
        Err(LedgerError::Store(_))
    ));
    assert_eq!(l.state(), &state);
    assert_eq!(state_of(&l, "a"), State::Delivered);
}

#[test]
fn a_refused_transition_writes_nothing() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    let before = store.bytes();
    assert!(l.confirm(&id("a")).is_err());
    assert!(l.unblock(&id("a")).is_err());
    assert!(l.begin_delivery(&id("nope"), code("r")).is_err());
    assert_eq!(store.bytes(), before);
    assert!(
        !l.is_failed(),
        "a refused transition is not a storage failure"
    );
}

#[test]
fn file_store_append_fails_cleanly_on_a_file_that_cannot_be_written() {
    let dir = TmpDir::new("ro");
    let path = dir.file("log");
    {
        let (mut l, _) = Ledger::open(FileStore::open(&path).unwrap(), ticking()).unwrap();
        l.create(new_dispatch("a", "s1", 0, &[])).unwrap();
    }
    let before = fs::read(&path).unwrap();
    // A read-only descriptor: every write fails.
    let ro = fs::File::open(&path).unwrap();
    let (mut l, _) = Ledger::open(FileStore::over(ro), ticking()).unwrap();
    let err = l.begin_delivery(&id("a"), code("r")).unwrap_err();
    assert!(matches!(err, LedgerError::Store(_)));
    assert_eq!(state_of_file(&l, "a"), State::Ready);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "no partial record on disk"
    );
}

fn state_of_file(l: &Ledger<FileStore>, name: &str) -> State {
    l.get(&id(name)).unwrap().state
}

// ---- the file backend ------------------------------------------------------

#[test]
fn file_store_round_trip_mode_and_single_writer() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TmpDir::new("file");
    let path = dir.file("ledger.log");
    let (mut l, rec) = Ledger::open(FileStore::open(&path).unwrap(), system_clock()).unwrap();
    assert_eq!(rec.records, 0);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    deliver_file(&mut l, "a");

    // A second writer on the same log is refused while the first is open.
    let err = FileStore::open(&path).err().expect("second open must fail");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    drop(l);

    let (l2, rec2) = Ledger::open(FileStore::open(&path).unwrap(), system_clock()).unwrap();
    assert_eq!(rec2.records, 3);
    assert_eq!(l2.get(&id("a")).unwrap().state, State::Delivered);
}

fn deliver_file(l: &mut Ledger<FileStore>, name: &str) {
    let next = l.get(&id(name)).unwrap().attempt + 1;
    l.begin_delivery(&id(name), code(&format!("req{next}")))
        .unwrap();
    l.confirm(&id(name)).unwrap();
}

#[test]
fn file_store_cuts_a_torn_tail_on_disk() {
    let dir = TmpDir::new("torn");
    let path = dir.file("ledger.log");
    {
        let (mut l, _) = Ledger::open(FileStore::open(&path).unwrap(), system_clock()).unwrap();
        l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    }
    let good = fs::read(&path).unwrap();
    let mut torn = good.clone();
    torn.extend_from_slice(b"{\"seq\":2,\"ty");
    fs::write(&path, &torn).unwrap();
    let (mut l, rec) = Ledger::open(FileStore::open(&path).unwrap(), system_clock()).unwrap();
    assert_eq!(rec.torn_bytes, 12);
    assert_eq!(fs::read(&path).unwrap(), good);
    l.begin_delivery(&id("a"), code("r")).unwrap();
    let r = replay(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(r.records, 2);
    assert_eq!(r.torn_bytes, 0);
}

#[test]
fn pure_matcher_checks_the_dispatch_id_itself() {
    let store = MemStore::default();
    let (mut l, _) = open_mem(&store);
    l.create(new_dispatch("a", "s1", 1, &[])).unwrap();
    l.begin_delivery(&id("a"), code("r1")).unwrap();
    l.confirm(&id("a")).unwrap();
    let d = l.get(&id("a")).unwrap();
    assert_eq!(
        match_record(Some(d), &claim("other", 1, "s1", Outcome::Done)),
        Err(RejectReason::UnknownDispatch)
    );
    assert_eq!(
        match_record(Some(d), &claim("a", 1, "s1", Outcome::Done)),
        Ok(Outcome::Done)
    );
}
