//! Test rig: a holder serving a [`LocalHost`] with a tail, and fronts that
//! reach it over real socketpairs. A front "dies" by dropping its client; the
//! holder and its panes live on, and the next `connect` is a new front.

use super::client::HolderClient;
use super::local::LocalHost;
use super::server::{Holder, ServeError};
use super::tail::TAIL_CAP;
use super::{
    Attached, HolderHost, HostEvents, HostPane, KillOutcome, PtyHost, SpawnReq, Spawned, TailRead,
};
use crate::backend::BackendError;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) struct Held {
    /// The holder's host: where the panes really are.
    pub local: Arc<LocalHost>,
    pub holder: Arc<Holder>,
    servers: Mutex<Vec<JoinHandle<Result<(), ServeError>>>>,
}

impl Held {
    /// A holder over a host with the production tail size.
    pub(crate) fn new() -> Arc<Self> {
        Self::with_tail(TAIL_CAP)
    }

    pub(crate) fn with_tail(cap: usize) -> Arc<Self> {
        Self::over(Arc::new(LocalHost::with_tail(cap)))
    }

    pub(crate) fn over(local: Arc<LocalHost>) -> Arc<Self> {
        let holder = Arc::new(Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>));
        Self::with_holder(local, holder)
    }

    pub(crate) fn with_holder(local: Arc<LocalHost>, holder: Arc<Holder>) -> Arc<Self> {
        Arc::new(Self {
            local,
            holder,
            servers: Mutex::new(Vec::new()),
        })
    }

    /// The two socketpairs of a link, and the holder serving its end.
    pub(crate) fn serve_pair(&self) -> (UnixStream, UnixStream) {
        let (front_ops, holder_ops) = UnixStream::pair().expect("socketpair");
        let (front_events, holder_events) = UnixStream::pair().expect("socketpair");
        let holder = Arc::clone(&self.holder);
        let server = std::thread::Builder::new()
            .name("test-holder-serve".to_string())
            .spawn(move || holder.serve(holder_ops, holder_events))
            .expect("serve thread");
        self.servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(server);
        (front_ops, front_events)
    }

    /// Wait for every serve call made so far to end and give their results.
    /// Call after the fronts are gone.
    pub(crate) fn finish_servers(&self) -> Vec<Result<(), ServeError>> {
        let servers =
            std::mem::take(&mut *self.servers.lock().unwrap_or_else(PoisonError::into_inner));
        servers
            .into_iter()
            .map(|s| s.join().expect("the serve thread panicked"))
            .collect()
    }

    /// Wait until every link served so far has ended: after the last front
    /// is gone, nothing it sent is still being handled. False when one was
    /// still being served after `limit`.
    pub(crate) fn links_ended(&self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        loop {
            let done = self
                .servers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .all(JoinHandle::is_finished);
            if done {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A new front: a client over a new link. It replaces the previous one.
    pub(crate) fn connect(self: &Arc<Self>) -> Arc<HolderClient> {
        let (ops, events) = self.serve_pair();
        let client = HolderClient::connect(ops, events).expect("connect");
        client.keep(Arc::clone(self));
        Arc::new(client)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Panes outlive a front, not the test that made them.
        super::PtyHost::end_all(&*self.local);
    }
}

/// A gate on the holder's writes: while armed, a `try_write` waits until the
/// test opens it. Lets a test hold a front's `write` reply back.
#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    armed: bool,
    open: bool,
    entered: usize,
}

impl Gate {
    pub(crate) fn arm(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *st = GateState {
            armed: true,
            open: false,
            entered: 0,
        };
    }

    pub(crate) fn open(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open = true;
        self.changed.notify_all();
    }

    /// Wait until `n` writes are held at the gate.
    pub(crate) fn wait_entered(&self, n: usize, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while st.entered < n {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn pass(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !st.armed {
            return;
        }
        st.entered += 1;
        self.changed.notify_all();
        while !st.open {
            st = self
                .changed
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A [`LocalHost`] whose `try_write` waits at a [`Gate`] while it is armed.
pub(crate) struct GateHost {
    pub inner: Arc<LocalHost>,
    pub gate: Arc<Gate>,
}

impl GateHost {
    pub(crate) fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(LocalHost::with_tail(cap)),
            gate: Arc::new(Gate::default()),
        })
    }

    /// A rig whose holder serves this host.
    pub(crate) fn rig(self: &Arc<Self>) -> Arc<Held> {
        let holder = Arc::new(Holder::new(Arc::clone(self) as Arc<dyn HolderHost>));
        Held::with_holder(Arc::clone(&self.inner), holder)
    }
}

impl PtyHost for GateHost {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        self.inner.set_sink(sink);
    }
    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        self.inner.spawn(req)
    }
    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        self.gate.pass();
        self.inner.try_write(id, bytes)
    }
    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        self.inner.wait_writable(id, timeout)
    }
    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        self.inner.resize(id, cols, rows)
    }
    fn signal(&self, id: &str, signal: i32) -> io::Result<()> {
        self.inner.signal(id, signal)
    }
    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome> {
        self.inner.kill(id, grace)
    }
    fn fg_pgrp(&self, id: &str) -> Option<i32> {
        self.inner.fg_pgrp(id)
    }
    fn list(&self) -> Vec<HostPane> {
        self.inner.list()
    }
    fn forget(&self, id: &str) {
        self.inner.forget(id);
    }
    fn end_all(&self) {
        self.inner.end_all();
    }
    fn attach(&self, id: &str) -> io::Result<Attached> {
        self.inner.attach(id)
    }
    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        self.inner.meta_set(id, bytes)
    }
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        self.inner.meta_get(id)
    }
    fn survives_restart(&self) -> bool {
        true
    }
}

impl HolderHost for GateHost {
    fn tail_read(&self, id: &str, from: u64, max: usize) -> io::Result<TailRead> {
        self.inner.tail_read(id, from, max)
    }
    fn tail_bounds(&self, id: &str) -> io::Result<(u64, u64)> {
        self.inner.tail_bounds(id)
    }
}
