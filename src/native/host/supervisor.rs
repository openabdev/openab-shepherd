//! The holder process (`openab-shepherd hold`, issue #76): it owns the panes
//! and keeps one front alive.
//!
//! One supervisor loop, on the main thread, owns the front child. Everything
//! that can happen to it arrives as an [`Event`] on one channel: a signal (read
//! by a thread that waits for the blocked signals, the self-pipe of a
//! single-threaded program) or the front's exit (read by a watcher thread that
//! waits without reaping, so the pid cannot be reused before the loop has
//! decided what to do). The loop reaps the child itself, so there is never a
//! `<defunct>` front.
//!
//! - The front dies for any reason: respawn after a backoff (1 s, doubling to
//!   30 s, reset once a front has stayed up 60 s). The holder never exits
//!   because of how a front ended.
//! - SIGUSR2: SIGTERM the front (SIGKILL after 5 s) and respawn at once, no
//!   backoff. Panes are untouched.
//! - SIGTERM, SIGINT, SIGHUP: stop the front, end every pane, exit 128+sig.
//! - A panic on the main thread ends every pane (as the single process does).
//! - The holder's own death (SIGKILL) closes every PTY master, which hangs up
//!   the panes. Nothing here keeps a master open outside the holder.

use super::local::LocalHost;
use super::pty::{self, default_signals_on_exec, SPAWN_LOCK};
use super::server::Holder;
use super::tail::TAIL_CAP;
use super::{HolderHost, PtyHost};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Where the front finds its two link ends (the child ends are moved here
/// before exec, whatever numbers they had in the holder).
pub(crate) const FRONT_OPS_FD: RawFd = 3;
pub(crate) const FRONT_EVENTS_FD: RawFd = 4;

/// The timings of the supervisor. A struct so tests run it in milliseconds.
#[derive(Debug, Clone)]
pub(crate) struct Timing {
    /// The first respawn delay after an unplanned exit.
    pub initial: Duration,
    /// The longest respawn delay.
    pub max: Duration,
    /// A front that stayed up this long starts the delays over.
    pub stable: Duration,
    /// How long a front has to leave on SIGTERM before SIGKILL.
    pub term_grace: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            stable: Duration::from_secs(60),
            term_grace: Duration::from_secs(5),
        }
    }
}

/// The respawn delays after unplanned exits.
#[derive(Debug)]
pub(crate) struct Backoff {
    timing: Timing,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(timing: &Timing) -> Self {
        Self {
            timing: timing.clone(),
            next: timing.initial,
        }
    }

    /// The delay before the next front, after one that was up for `up_for`.
    pub(crate) fn after_exit(&mut self, up_for: Duration) -> Duration {
        if up_for >= self.timing.stable {
            self.next = self.timing.initial;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(self.timing.max);
        delay
    }
}

/// What reaches the supervisor loop.
#[derive(Debug)]
pub(crate) enum Event {
    Signal(i32),
    /// The front of generation `gen` has exited and is not yet reaped.
    Exited {
        gen: u64,
    },
}

/// What the loop does to the outside world: tests give it a fake.
pub(crate) trait FrontControl {
    /// Start front number `gen`; the child's exit arrives as
    /// `Event::Exited { gen }` on `events`. Returns its pid.
    fn launch(&mut self, gen: u64, events: Sender<Event>) -> io::Result<i32>;
    fn signal(&mut self, pid: i32, signal: i32);
    /// Reap the exited front `gen`; how it ended, for the log.
    fn reap(&mut self, gen: u64) -> String;
    /// End every pane (the holder is exiting).
    fn end_panes(&mut self);
}

enum State {
    Running {
        gen: u64,
        pid: i32,
        started: Instant,
        stopping: Option<Stopping>,
    },
    Waiting {
        until: Instant,
    },
}

struct Stopping {
    kill_at: Instant,
    killed: bool,
    /// The stop is a SIGUSR2 restart: respawn at once.
    restart: bool,
}

/// The supervisor loop. Returns the holder's exit status (128 + the signal
/// that ended it) once the front is gone and the panes are ended.
pub(crate) fn supervise(
    front: &mut dyn FrontControl,
    events: &Receiver<Event>,
    sender: &Sender<Event>,
    timing: &Timing,
    log: &dyn Fn(&str),
) -> i32 {
    let mut backoff = Backoff::new(timing);
    let mut gen = 0u64;
    let mut quit: Option<i32> = None;

    let start = |front: &mut dyn FrontControl, gen: &mut u64, backoff: &mut Backoff| {
        *gen += 1;
        match front.launch(*gen, sender.clone()) {
            Ok(pid) => {
                log(&format!("front {gen} started (pid {pid})"));
                State::Running {
                    gen: *gen,
                    pid,
                    started: Instant::now(),
                    stopping: None,
                }
            }
            Err(err) => {
                let delay = backoff.after_exit(Duration::ZERO);
                log(&format!(
                    "cannot start the front: {err}; trying again in {} ms",
                    delay.as_millis()
                ));
                State::Waiting {
                    until: Instant::now() + delay,
                }
            }
        }
    };

    let mut state = start(front, &mut gen, &mut backoff);
    loop {
        let wake = match &state {
            State::Running {
                stopping: Some(s), ..
            } if !s.killed => Some(s.kill_at),
            State::Waiting { until } => Some(*until),
            State::Running { .. } => None,
        };
        let event = match wake {
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => Some(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match events.recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            },
        };
        match event {
            Some(Event::Signal(sig)) if sig == libc::SIGUSR2 => {
                if quit.is_some() {
                    continue;
                }
                match &mut state {
                    State::Running {
                        pid,
                        stopping: stopping @ None,
                        ..
                    } => {
                        log("SIGUSR2: restarting the front, panes untouched");
                        front.signal(*pid, libc::SIGTERM);
                        *stopping = Some(Stopping {
                            kill_at: Instant::now() + timing.term_grace,
                            killed: false,
                            restart: true,
                        });
                    }
                    State::Running { .. } => {}
                    State::Waiting { until } => {
                        log("SIGUSR2: starting the front now");
                        *until = Instant::now();
                    }
                }
            }
            Some(Event::Signal(sig)) => {
                if quit.is_some() {
                    continue;
                }
                quit = Some(128 + sig);
                log(&format!(
                    "signal {sig}: stopping the front and ending panes"
                ));
                match &mut state {
                    State::Running { pid, stopping, .. } => {
                        if stopping.is_none() {
                            front.signal(*pid, libc::SIGTERM);
                        }
                        *stopping = Some(Stopping {
                            kill_at: Instant::now() + timing.term_grace,
                            killed: false,
                            restart: false,
                        });
                    }
                    State::Waiting { .. } => break,
                }
            }
            Some(Event::Exited { gen: exited }) => {
                let State::Running {
                    gen: current,
                    started,
                    stopping,
                    ..
                } = &state
                else {
                    continue;
                };
                if *current != exited {
                    continue;
                }
                let up_for = started.elapsed();
                let planned_restart = stopping.as_ref().is_some_and(|s| s.restart);
                let how = front.reap(exited);
                log(&format!("front {exited} exited: {how}"));
                if quit.is_some() {
                    break;
                }
                if planned_restart {
                    state = start(front, &mut gen, &mut backoff);
                } else {
                    let delay = backoff.after_exit(up_for);
                    log(&format!("respawning the front in {} ms", delay.as_millis()));
                    state = State::Waiting {
                        until: Instant::now() + delay,
                    };
                }
            }
            None => match &mut state {
                State::Running {
                    pid,
                    stopping: Some(s),
                    ..
                } if !s.killed && Instant::now() >= s.kill_at => {
                    log("the front ignored SIGTERM: SIGKILL");
                    front.signal(*pid, libc::SIGKILL);
                    s.killed = true;
                }
                State::Waiting { until } if Instant::now() >= *until => {
                    state = start(front, &mut gen, &mut backoff);
                }
                _ => {}
            },
        }
    }
    front.end_panes();
    quit.unwrap_or(1)
}

// ---------------------------------------------------------------------------
// The real front: a child process of this binary
// ---------------------------------------------------------------------------

/// The first line a front wrote to stderr, which tells why a front that exits
/// at once did so.
type FirstLine = Arc<Mutex<Option<String>>>;

struct Running {
    child: Child,
    first_line: FirstLine,
    /// The stderr forwarder is done (the pipe closed).
    forwarded: Receiver<()>,
}

struct RealFront {
    exe: OsString,
    args: Vec<String>,
    holder: Arc<Holder>,
    local: Arc<LocalHost>,
    running: HashMap<u64, Running>,
}

/// `fd` copied to a descriptor above the ones the front will find its link
/// on, so moving them there never overwrites the other end.
fn dup_high(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC on a descriptor this process owns.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if new == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `new` is a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

impl FrontControl for RealFront {
    fn launch(&mut self, gen: u64, events: Sender<Event>) -> io::Result<i32> {
        // Held from the socketpairs until the child ends are closed again, so
        // no pane spawn forks while they are inheritable (macOS sets
        // close-on-exec after the socket exists, not atomically).
        let _lock = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let (holder_ops, front_ops) = UnixStream::pair()?;
        let (holder_events, front_events) = UnixStream::pair()?;
        let ops_src = dup_high(front_ops.as_raw_fd())?;
        let events_src = dup_high(front_events.as_raw_fd())?;
        let (ops_raw, events_raw) = (ops_src.as_raw_fd(), events_src.as_raw_fd());
        let mut command = Command::new(&self.exe);
        command
            .arg("front")
            .arg("--ops-fd")
            .arg(FRONT_OPS_FD.to_string())
            .arg("--events-fd")
            .arg(FRONT_EVENTS_FD.to_string())
            .args(&self.args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        default_signals_on_exec(&mut command);
        // SAFETY: runs in the forked child before exec and calls only
        // async-signal-safe functions (dup2); dup2 clears close-on-exec on
        // the new descriptor.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(ops_raw, FRONT_OPS_FD) == -1
                    || libc::dup2(events_raw, FRONT_EVENTS_FD) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(command);
        drop((ops_src, events_src, front_ops, front_events));
        drop(_lock);
        let pid = child.id() as i32;

        let first_line: FirstLine = Arc::new(Mutex::new(None));
        let (done_tx, forwarded) = channel();
        if let Some(stderr) = child.stderr.take() {
            let first = Arc::clone(&first_line);
            thread::Builder::new()
                .name("front-stderr".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        match reader.read_until(b'\n', &mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let text = String::from_utf8_lossy(&line);
                                let text = text.trim_end_matches(['\n', '\r']);
                                eprintln!("{text}");
                                let mut slot = first.lock().unwrap_or_else(PoisonError::into_inner);
                                if slot.is_none() {
                                    *slot = Some(text.to_string());
                                }
                            }
                        }
                    }
                    let _ = done_tx.send(());
                })?;
        }
        let holder = Arc::clone(&self.holder);
        thread::Builder::new()
            .name("front-serve".to_string())
            .spawn(move || {
                // The holder logs why a link ends; a version mismatch is
                // also the front's exit status.
                let _ = holder.serve(holder_ops, holder_events);
            })?;
        thread::Builder::new()
            .name("front-wait".to_string())
            .spawn(move || {
                let _ = pty::wait_exited(pid);
                let _ = events.send(Event::Exited { gen });
            })?;
        self.running.insert(
            gen,
            Running {
                child,
                first_line,
                forwarded,
            },
        );
        Ok(pid)
    }

    fn signal(&mut self, pid: i32, signal: i32) {
        // SAFETY: the supervisor signals only a front it has not reaped, so
        // the pid is still that child's.
        unsafe {
            libc::kill(pid, signal);
        }
    }

    fn reap(&mut self, gen: u64) -> String {
        let Some(mut running) = self.running.remove(&gen) else {
            return "unknown".to_string();
        };
        let status = match running.child.wait() {
            Ok(status) => status.to_string(),
            Err(err) => format!("cannot read the status: {err}"),
        };
        // The forwarder ends when the pipe closes; a grandchild that kept it
        // open must not hold the holder up.
        let _ = running.forwarded.recv_timeout(Duration::from_millis(500));
        let first = running
            .first_line
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match first {
            Some(line) => format!("{status}; first stderr line: {line:?}"),
            None => format!("{status}; no stderr output"),
        }
    }

    fn end_panes(&mut self) {
        PtyHost::end_all(&*self.local);
    }
}

fn log_line(msg: &str) {
    eprintln!("openab-shepherd: holder: {msg}");
}

/// Block the signals the holder acts on, in this thread and every thread it
/// starts later.
fn block_holder_signals() -> io::Result<libc::sigset_t> {
    // SAFETY: sigset operations on a local set, then this thread's mask.
    unsafe {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(set.as_mut_ptr());
        let mut set = set.assume_init();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGUSR2] {
            libc::sigaddset(&mut set, signal);
        }
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        Ok(set)
    }
}

/// Run the holder: `front_args` go to every front after the link flags.
/// Returns the process exit status. Call it first in `main`, before any
/// thread exists.
pub fn run_holder(front_args: Vec<String>) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(path) => path.into_os_string(),
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: cannot find this executable: {err}");
            return 2;
        }
    };
    let set = match block_holder_signals() {
        Ok(set) => set,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: cannot block signals ({err})");
            return 2;
        }
    };
    let local = Arc::new(LocalHost::with_tail(TAIL_CAP));
    let holder = Arc::new(Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>));

    // A panic on the main thread ends the panes, as the single process does.
    let on_panic = Arc::clone(&local);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if thread::current().name() == Some("main") {
            PtyHost::end_all(&*on_panic);
        }
    }));

    let (sender, events) = channel();
    {
        let sender = sender.clone();
        let spawned = thread::Builder::new()
            .name("holder-signals".to_string())
            .spawn(move || loop {
                let signal = pty::wait_exit_signal(&set);
                if sender.send(Event::Signal(signal)).is_err() {
                    return;
                }
            });
        if let Err(err) = spawned {
            eprintln!("openab-shepherd: refusing to start: cannot start the signal thread ({err})");
            return 2;
        }
    }
    log_line(&format!(
        "started (pid {}, protocol {})",
        std::process::id(),
        super::wire::HOLDER_VERSION
    ));
    let mut front = RealFront {
        exe,
        args: front_args,
        holder,
        local,
        running: HashMap::new(),
    };
    let code = supervise(&mut front, &events, &sender, &Timing::default(), &log_line);
    log_line(&format!("exiting with status {code}"));
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_delays_double_to_the_maximum() {
        let timing = Timing::default();
        let mut backoff = Backoff::new(&timing);
        let seq: Vec<u64> = (0..8)
            .map(|_| backoff.after_exit(ms(10)).as_secs())
            .collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_front_that_stayed_up_starts_the_delays_over() {
        let timing = Timing::default();
        let mut backoff = Backoff::new(&timing);
        for _ in 0..4 {
            backoff.after_exit(ms(10));
        }
        assert_eq!(backoff.after_exit(Duration::from_secs(59)).as_secs(), 16);
        assert_eq!(backoff.after_exit(Duration::from_secs(60)).as_secs(), 1);
        assert_eq!(backoff.after_exit(ms(5)).as_secs(), 2);
    }

    /// A front control with no processes: each launch is a pid, and the test
    /// decides when it exits.
    #[derive(Default)]
    struct Fake {
        launched: Vec<(u64, Instant)>,
        signals: Vec<(i32, i32)>,
        ended: Arc<AtomicBool>,
        launch_fails: AtomicUsize,
        ignore_term: bool,
        events: Option<Sender<Event>>,
    }

    impl FrontControl for Fake {
        fn launch(&mut self, gen: u64, events: Sender<Event>) -> io::Result<i32> {
            if self.launch_fails.load(Ordering::Relaxed) > 0 {
                self.launch_fails.fetch_sub(1, Ordering::Relaxed);
                return Err(io::Error::other("no such file"));
            }
            self.launched.push((gen, Instant::now()));
            self.events = Some(events);
            Ok(1000 + gen as i32)
        }
        fn signal(&mut self, pid: i32, signal: i32) {
            self.signals.push((pid, signal));
            // A front that obeys SIGTERM, or any front on SIGKILL, exits.
            if signal == libc::SIGKILL || (signal == libc::SIGTERM && !self.ignore_term) {
                let gen = (pid - 1000) as u64;
                let _ = self.events.as_ref().unwrap().send(Event::Exited { gen });
            }
        }
        fn reap(&mut self, _gen: u64) -> String {
            "exit status: 0".to_string()
        }
        fn end_panes(&mut self) {
            self.ended.store(true, Ordering::Release);
        }
    }

    fn quick() -> Timing {
        Timing {
            initial: ms(40),
            max: ms(160),
            stable: ms(10_000),
            term_grace: ms(100),
        }
    }

    struct Harness {
        tx: Sender<Event>,
        thread: thread::JoinHandle<(Fake, i32, Vec<String>)>,
    }

    fn start(timing: Timing, mut fake: Fake) -> Harness {
        let (tx, rx) = channel();
        let sender = tx.clone();
        let thread = thread::spawn(move || {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&lines);
            let log = move |m: &str| sink.lock().unwrap().push(m.to_string());
            let code = supervise(&mut fake, &rx, &sender, &timing, &log);
            let lines = lines.lock().unwrap().clone();
            (fake, code, lines)
        });
        Harness { tx, thread }
    }

    #[test]
    fn an_unplanned_exit_respawns_after_the_backoff_and_it_grows() {
        let h = start(quick(), Fake::default());
        thread::sleep(ms(30));
        for gen in 1..=3u64 {
            h.tx.send(Event::Exited { gen }).unwrap();
            thread::sleep(ms(300));
        }
        h.tx.send(Event::Signal(libc::SIGTERM)).unwrap();
        let (fake, code, lines) = h.thread.join().unwrap();
        assert_eq!(code, 128 + libc::SIGTERM);
        assert!(fake.ended.load(Ordering::Acquire));
        let gaps: Vec<u128> = fake
            .launched
            .windows(2)
            .map(|w| w[1].1.duration_since(w[0].1).as_millis())
            .collect();
        assert_eq!(fake.launched.len(), 4, "{lines:?}");
        // Each front starts no sooner than its delay after the last exit.
        assert!(gaps[0] >= 40 && gaps[1] >= 80 && gaps[2] >= 160, "{gaps:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("respawning the front in 40 ms"))
                && lines.iter().any(|l| l.contains("in 80 ms"))
                && lines.iter().any(|l| l.contains("in 160 ms")),
            "{lines:?}"
        );
    }

    #[test]
    fn sigusr2_restarts_the_front_at_once_with_no_backoff_and_ends_no_pane() {
        let h = start(
            Timing {
                initial: Duration::from_secs(30),
                ..quick()
            },
            Fake::default(),
        );
        thread::sleep(ms(30));
        for _ in 0..3 {
            h.tx.send(Event::Signal(libc::SIGUSR2)).unwrap();
            thread::sleep(ms(80));
        }
        h.tx.send(Event::Signal(libc::SIGINT)).unwrap();
        let (fake, code, lines) = h.thread.join().unwrap();
        assert_eq!(code, 128 + libc::SIGINT);
        // Four fronts in well under one backoff step (30 s).
        assert_eq!(fake.launched.len(), 4, "{lines:?}");
        assert!(
            !lines.iter().any(|l| l.contains("respawning")),
            "no backoff: {lines:?}"
        );
        // Each restart sent SIGTERM to the running front, nothing else.
        let terms = fake
            .signals
            .iter()
            .filter(|(_, s)| *s == libc::SIGTERM)
            .count();
        assert_eq!(terms, 4, "three restarts and the final stop");
        assert!(fake.signals.iter().all(|(_, s)| *s == libc::SIGTERM));
    }

    #[test]
    fn sigusr2_while_the_front_waits_for_its_backoff_starts_it_now() {
        let h = start(
            Timing {
                initial: Duration::from_secs(30),
                ..quick()
            },
            Fake::default(),
        );
        thread::sleep(ms(30));
        h.tx.send(Event::Exited { gen: 1 }).unwrap();
        thread::sleep(ms(60));
        h.tx.send(Event::Signal(libc::SIGUSR2)).unwrap();
        thread::sleep(ms(100));
        h.tx.send(Event::Signal(libc::SIGTERM)).unwrap();
        let (fake, _, lines) = h.thread.join().unwrap();
        assert_eq!(fake.launched.len(), 2, "{lines:?}");
    }

    #[test]
    fn a_front_that_ignores_sigterm_gets_sigkill_after_the_grace() {
        let h = start(
            quick(),
            Fake {
                ignore_term: true,
                ..Fake::default()
            },
        );
        thread::sleep(ms(30));
        h.tx.send(Event::Signal(libc::SIGUSR2)).unwrap();
        thread::sleep(ms(400));
        h.tx.send(Event::Signal(libc::SIGTERM)).unwrap();
        thread::sleep(ms(400));
        let (fake, _, lines) = h.thread.join().unwrap();
        assert_eq!(
            &fake.signals[..2],
            &[(1001, libc::SIGTERM), (1001, libc::SIGKILL)],
            "{lines:?}"
        );
        assert_eq!(fake.launched.len(), 2, "restarted once KILL landed");
        assert!(fake.signals.contains(&(1002, libc::SIGKILL)), "{lines:?}");
    }

    #[test]
    fn a_stop_signal_during_a_restart_ends_the_holder_and_the_panes() {
        // A front that ignores SIGTERM, so the restart is still under way
        // when the stop arrives, whatever the scheduling.
        let h = start(
            quick(),
            Fake {
                ignore_term: true,
                ..Fake::default()
            },
        );
        thread::sleep(ms(30));
        h.tx.send(Event::Signal(libc::SIGUSR2)).unwrap();
        h.tx.send(Event::Signal(libc::SIGHUP)).unwrap();
        let (fake, code, _) = h.thread.join().unwrap();
        assert_eq!(code, 128 + libc::SIGHUP);
        assert!(fake.ended.load(Ordering::Acquire));
        assert_eq!(fake.launched.len(), 1, "no front after the stop");
    }

    #[test]
    fn a_stop_while_waiting_for_the_backoff_ends_the_panes() {
        let h = start(
            Timing {
                initial: Duration::from_secs(30),
                ..quick()
            },
            Fake::default(),
        );
        thread::sleep(ms(30));
        h.tx.send(Event::Exited { gen: 1 }).unwrap();
        thread::sleep(ms(50));
        h.tx.send(Event::Signal(libc::SIGTERM)).unwrap();
        let (fake, code, _) = h.thread.join().unwrap();
        assert_eq!(code, 143);
        assert!(fake.ended.load(Ordering::Acquire));
    }

    #[test]
    fn a_front_that_cannot_start_is_tried_again_and_the_holder_stays() {
        let h = start(
            quick(),
            Fake {
                launch_fails: AtomicUsize::new(2),
                ..Fake::default()
            },
        );
        thread::sleep(ms(400));
        h.tx.send(Event::Signal(libc::SIGTERM)).unwrap();
        let (fake, code, lines) = h.thread.join().unwrap();
        assert_eq!(code, 143);
        assert_eq!(fake.launched.len(), 1, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("cannot start the front")));
    }

    #[test]
    fn the_front_is_forked_under_the_spawn_lock() {
        // A pane spawn holds SPAWN_LOCK from `openpty` until its descriptors
        // are close-on-exec; the link's socketpairs need the same cover.
        let local = Arc::new(LocalHost::with_tail(1024));
        let holder = Arc::new(Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>));
        let mut front = RealFront {
            exe: OsString::from("/usr/bin/true"),
            args: Vec::new(),
            holder,
            local,
            running: HashMap::new(),
        };
        let (tx, rx) = channel();
        let guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let launcher = thread::spawn(move || {
            let pid = front.launch(1, tx).expect("launch");
            (front, pid)
        });
        thread::sleep(ms(300));
        assert!(!launcher.is_finished(), "launch did not wait for the lock");
        drop(guard);
        let (mut front, pid) = launcher.join().unwrap();
        assert!(pid > 0);
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            Event::Exited { gen } => assert_eq!(gen, 1),
            other => panic!("{other:?}"),
        }
        assert!(front.reap(1).contains("exit status: 0"));
    }
}
