//! The PTY layer (ADR §3, §10, §11): `openpty`, spawning a child as its own
//! session with the slave as its controlling terminal, the child's
//! environment, and non-blocking I/O and signal helpers on the master. Every
//! `unsafe` block of the native core is here, except the process-table reads
//! in `procs.rs`.

use super::term::{COLS, ROWS};
use super::MAX_EXTRA_ENV;
use crate::backend::{BackendError, ErrorKind};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Held from `openpty` until `spawn` returns, so no other native spawn can
/// fork while this pane's descriptors are still inheritable. Any other code
/// in this process that forks while panes may be starting must take it too
/// (the keychain read in `profiles.rs` does), or it can inherit a pane's
/// descriptors in the window between `openpty` and the close-on-exec flag.
pub(crate) static SPAWN_LOCK: Mutex<()> = Mutex::new(());

const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const DEFAULT_LANG: &str = "en_US.UTF-8";
const TERM: &str = "xterm-256color";
/// Names a per-run environment may not set: the fixed baseline, and loader
/// variables.
const PROTECTED_KEYS: [&str; 5] = ["HOME", "PATH", "USER", "LANG", "TERM"];
const PROTECTED_PREFIXES: [&str; 2] = ["DYLD_", "LD_"];

/// A caller's per-run environment, checked: at most 32 entries, names
/// `[A-Z_][A-Z0-9_]*` and not protected, values without NUL.
pub(crate) fn run_env(env: &[(String, String)]) -> Result<Vec<(OsString, OsString)>, BackendError> {
    let out: Vec<(OsString, OsString)> = env
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    validate_extra(&out)?;
    for (key, _) in env {
        if PROTECTED_KEYS.contains(&key.as_str())
            || PROTECTED_PREFIXES.iter().any(|p| key.starts_with(p))
        {
            return Err(invalid(format!(
                "native: a per-run environment may not set {key}"
            )));
        }
    }
    Ok(out)
}

fn validate_extra(extra: &[(OsString, OsString)]) -> Result<(), BackendError> {
    if extra.len() > MAX_EXTRA_ENV {
        return Err(invalid(format!(
            "native: at most {MAX_EXTRA_ENV} extra environment variables are allowed"
        )));
    }
    for (key, value) in extra {
        if !is_env_key(key) {
            return Err(invalid(
                "native: an extra environment key must match [A-Z_][A-Z0-9_]*",
            ));
        }
        if value.as_bytes().contains(&0) {
            return Err(invalid("native: an extra environment value contains NUL"));
        }
    }
    Ok(())
}

/// The child's whole environment: `HOME`, `PATH`, `USER` and `LANG` from
/// `lookup` (with defaults for `PATH` and `LANG`), `TERM=xterm-256color`,
/// then `extra`. Nothing else from the parent passes; a credential reaches a
/// child only as `extra`, from a launch profile (`profiles.rs`).
pub(crate) fn child_env(
    lookup: impl Fn(&str) -> Option<OsString>,
    extra: &[(OsString, OsString)],
) -> Result<Vec<(OsString, OsString)>, BackendError> {
    validate_extra(extra)?;
    let parent = |key: &str| lookup(key).filter(|value| !value.is_empty());
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    if let Some(home) = parent("HOME") {
        env.push(("HOME".into(), home));
    }
    env.push((
        "PATH".into(),
        parent("PATH").unwrap_or_else(|| DEFAULT_PATH.into()),
    ));
    if let Some(user) = parent("USER") {
        env.push(("USER".into(), user));
    }
    env.push((
        "LANG".into(),
        parent("LANG").unwrap_or_else(|| DEFAULT_LANG.into()),
    ));
    env.push(("TERM".into(), TERM.into()));
    for (key, value) in extra {
        env.retain(|(k, _)| k != key);
        env.push((key.clone(), value.clone()));
    }
    Ok(env)
}

fn is_env_key(key: &OsStr) -> bool {
    let bytes = key.as_bytes();
    bytes
        .first()
        .is_some_and(|&b| b.is_ascii_uppercase() || b == b'_')
        && bytes
            .iter()
            .all(|&b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn invalid(detail: impl Into<String>) -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, detail)
}

/// A started child: the PTY master (non-blocking, close-on-exec) and the
/// process, which is its own session and group leader (pgid = pid). The
/// parent's slave copy stays open in `slave` until the reaper sees the leader
/// exit (issue #54: on macOS, closing it right after spawn can lose the
/// output of a program that prints and exits at once).
pub(crate) struct Spawned {
    pub master: OwnedFd,
    pub slave: OwnedFd,
    pub child: Child,
}

/// Start `argv` (non-empty) in `cwd` with exactly `env`, on a fresh PTY.
pub(crate) fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<Spawned, BackendError> {
    let _lock = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    spawn_locked(argv, cwd, env)
}

/// `spawn` for a caller that already holds `SPAWN_LOCK`.
pub(crate) fn spawn_locked(
    argv: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<Spawned, BackendError> {
    let argv0 = argv.first().map_or("", String::as_str);
    let fail = |kind: ErrorKind, detail: String, err: &io::Error| {
        eprintln!("openab-shepherd: native: spawn failed: {argv0}: {err}");
        BackendError::new(kind, detail)
    };
    let (master, slave) = open_pty().map_err(|err| {
        fail(
            ErrorKind::Other,
            format!("native: openpty failed: {err}"),
            &err,
        )
    })?;
    let stdio = || slave.try_clone().map(Stdio::from);
    let (stdin, stdout, stderr) = match (stdio(), stdio(), stdio()) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => {
            return Err(fail(
                ErrorKind::Other,
                format!("native: spawn {argv0} failed: {err}"),
                &err,
            ))
        }
    };
    let mut command = Command::new(argv0);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    // Discover the descriptors to mark here, in the parent under SPAWN_LOCK,
    // as late as possible so only another thread opening a descriptor
    // without close-on-exec before the fork can escape it.
    let plan = match CloexecPlan::new() {
        Ok(plan) => plan,
        Err(err) => {
            return Err(fail(
                ErrorKind::Other,
                format!("native: spawn {argv0} failed: {err}"),
                &err,
            ))
        }
    };
    // SAFETY: `child_setup` runs in the forked child before exec and calls
    // only async-signal-safe functions on a plan built in the parent.
    unsafe {
        command.pre_exec(move || child_setup(&plan));
    }
    let spawned = command.spawn();
    // The Command holds the parent's copies of the slave until dropped. The
    // original `slave` is kept (see `Spawned`); on a spawn error it drops
    // with the early return below.
    drop(command);
    let child = spawned.map_err(|err| {
        let kind = if err.kind() == io::ErrorKind::NotFound {
            ErrorKind::NotFound
        } else {
            ErrorKind::Other
        };
        fail(kind, format!("native: spawn {argv0} failed: {err}"), &err)
    })?;
    Ok(Spawned {
        master,
        slave,
        child,
    })
}

/// Make `command`'s child start with default dispositions for the signals
/// Shepherd handles and an empty signal mask.
pub(crate) fn default_signals_on_exec(command: &mut Command) {
    // SAFETY: `default_signals` runs in the forked child before exec and
    // calls only async-signal-safe functions.
    unsafe {
        command.pre_exec(default_signals);
    }
}

/// Shepherd's effective user id.
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// In the child, after stdio is the slave: a new session, the slave as the
/// controlling terminal, every descriptor above 2 close-on-exec, default
/// signal dispositions and an empty mask.
fn child_setup(plan: &CloexecPlan) -> io::Result<()> {
    // SAFETY: plain libc calls on this process; all are async-signal-safe.
    unsafe {
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    plan.apply()?;
    default_signals()
}

/// Backstop for a hard descriptor limit of `RLIM_INFINITY`, and the largest
/// range the fallback scan will cover.
const FD_SCAN_BACKSTOP: libc::c_int = 1 << 20;

/// Test-only switch: skip `close_range` so the enumeration and the scan are
/// exercised on Linux too. Also set by `SHEPHERD_TEST_SKIP_CLOSE_RANGE`.
#[cfg(test)]
pub(crate) static SKIP_CLOSE_RANGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(all(test, target_os = "linux"))]
fn close_range_allowed() -> bool {
    !SKIP_CLOSE_RANGE.load(std::sync::atomic::Ordering::SeqCst)
        && std::env::var_os("SHEPHERD_TEST_SKIP_CLOSE_RANGE").is_none()
}

#[cfg(all(not(test), target_os = "linux"))]
fn close_range_allowed() -> bool {
    true
}

/// Which descriptors the child marks close-on-exec. Everything that needs
/// allocation, `sysconf` or `getrlimit` is decided in the parent (under
/// `SPAWN_LOCK`); the child only applies it.
struct CloexecPlan {
    /// Try `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` first (Linux >= 5.11).
    #[cfg(target_os = "linux")]
    close_range: bool,
    fds: FdSet,
}

enum FdSet {
    /// The parent's open descriptors above 2 at planning time.
    Listed(Vec<libc::c_int>),
    /// Enumeration failed: every descriptor from 3 below this bound.
    Scan(libc::c_int),
}

impl CloexecPlan {
    /// Parent side. Fails when neither the enumeration nor a scan bounded by
    /// the hard descriptor limit can cover every descriptor.
    fn new() -> io::Result<Self> {
        let fds = match list_open_descriptors() {
            Ok(list) => FdSet::Listed(list),
            Err(_) => FdSet::Scan(scan_bound()?),
        };
        Ok(Self {
            #[cfg(target_os = "linux")]
            close_range: close_range_allowed(),
            fds,
        })
    }

    /// Child side: async-signal-safe, no allocation. Marks instead of closing
    /// on purpose: `std::process::Command` keeps a close-on-exec pipe (a
    /// descriptor above 2) open during `pre_exec` to carry a failed `pre_exec`
    /// or `exec` back to the parent. Closing it here would make every such
    /// failure look like a successful spawn.
    fn apply(&self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if self.close_range {
            // SAFETY: a raw syscall on this process's descriptor table.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC as libc::c_uint,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            // ENOSYS, EINVAL or a seccomp refusal: use the descriptor list.
        }
        match &self.fds {
            FdSet::Listed(list) => {
                for &fd in list {
                    mark_cloexec(fd)?;
                }
            }
            FdSet::Scan(bound) => {
                for fd in 3..*bound {
                    mark_cloexec(fd)?;
                }
            }
        }
        Ok(())
    }
}

/// Set `FD_CLOEXEC` on `fd`. A descriptor that is not open is skipped; one
/// that `F_GETFD` accepts and `F_SETFD` then refuses fails the spawn.
fn mark_cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor number has no memory-safety preconditions.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return Ok(());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Exclusive upper bound for a scan: the hard `RLIMIT_NOFILE`, with a
/// backstop for an unlimited hard limit. A finite hard limit above the
/// backstop cannot be covered, so it is an error.
fn scan_bound() -> io::Result<libc::c_int> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-pointer for the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if limit.rlim_max == libc::RLIM_INFINITY {
        return Ok(FD_SCAN_BACKSTOP);
    }
    match libc::c_int::try_from(limit.rlim_max) {
        Ok(bound) if bound <= FD_SCAN_BACKSTOP => Ok(bound),
        _ => Err(io::Error::other(format!(
            "cannot list open descriptors and the hard descriptor limit {} is too large to scan",
            limit.rlim_max
        ))),
    }
}

/// The descriptors above 2 this process has open, from `/proc/self/fd`
/// (Linux) or `/dev/fd`, excluding the directory descriptor used to read it.
fn list_open_descriptors() -> io::Result<Vec<libc::c_int>> {
    #[cfg(target_os = "linux")]
    let path = c"/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let path = c"/dev/fd";
    // SAFETY: `path` is NUL-terminated; the DIR is closed on every path.
    unsafe {
        let dir = libc::opendir(path.as_ptr());
        if dir.is_null() {
            return Err(io::Error::last_os_error());
        }
        let own = libc::dirfd(dir);
        let mut fds = Vec::new();
        let mut result = Ok(());
        loop {
            *errno_location() = 0;
            let entry = libc::readdir(dir);
            if entry.is_null() {
                if *errno_location() != 0 {
                    result = Err(io::Error::last_os_error());
                }
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            match std::str::from_utf8(name)
                .ok()
                .and_then(|n| n.parse::<libc::c_int>().ok())
            {
                Some(fd) if fd > 2 => {
                    if fd != own {
                        fds.push(fd);
                    }
                }
                Some(_) => {}
                None => {
                    result = Err(io::Error::other("unparseable descriptor entry"));
                    break;
                }
            }
        }
        libc::closedir(dir);
        result.map(|()| fds)
    }
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno slot.
    unsafe { libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's errno slot.
    unsafe { libc::__error() }
}

/// Default dispositions for the signals Shepherd handles and an empty mask.
fn default_signals() -> io::Result<()> {
    // SAFETY: plain libc calls on this process; all are async-signal-safe.
    unsafe {
        for signal in [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGCHLD,
            libc::SIGALRM,
        ] {
            libc::signal(signal, libc::SIG_DFL);
        }
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(set.as_mut_ptr());
        if libc::sigprocmask(libc::SIG_SETMASK, set.as_ptr(), ptr::null_mut()) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let size = libc::winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: valid out-pointers; a null name and termios are allowed.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            ptr::null_mut(),
            ptr::null_mut::<libc::termios>(),
            // openpty takes *const winsize on Linux and *mut on macOS; it never writes through it.
            ptr::from_ref(&size).cast_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are open descriptors we now own.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_fd_flag(&master, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    set_fd_flag(&slave, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    set_fd_flag(&master, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
    Ok((master, slave))
}

fn set_fd_flag(
    fd: &OwnedFd,
    get: libc::c_int,
    set: libc::c_int,
    flag: libc::c_int,
) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), get);
        if flags == -1 || libc::fcntl(fd.as_raw_fd(), set, flags | flag) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn poll(fd: RawFd, events: libc::c_short, timeout_ms: libc::c_int) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(err)
        };
    }
    Ok(rc > 0)
}

/// Wait up to `timeout_ms` for output, then read once. `None` when nothing
/// was ready; `Some(0)` at end of file.
pub(crate) fn poll_read(
    fd: RawFd,
    buf: &mut [u8],
    timeout_ms: libc::c_int,
) -> io::Result<Option<usize>> {
    if !poll(fd, libc::POLLIN, timeout_ms)? {
        return Ok(None);
    }
    // SAFETY: `buf` is valid for `buf.len()` bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n >= 0 {
        return Ok(Some(n as usize));
    }
    let err = io::Error::last_os_error();
    match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
        _ => Err(err),
    }
}

/// One non-blocking write. `WouldBlock` when the PTY input buffer is full.
pub(crate) fn try_write(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` is valid for `bytes.len()` bytes.
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Wait up to `timeout` until the master accepts input.
pub(crate) fn poll_writable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let ms = timeout.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int;
    poll(fd, libc::POLLOUT, ms)
}

/// Signal the process group `pgid`. A group that no longer exists is not an
/// error. Refuses group ids 0 and 1, which would signal Shepherd's own group
/// or every process.
pub(crate) fn killpg(pgid: i32, signal: libc::c_int) -> io::Result<()> {
    if pgid <= 1 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // SAFETY: kill with a negative pid signals that group only.
    if unsafe { libc::kill(-pgid, signal) } == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }
    Ok(())
}

/// The terminal's foreground process group, read on the master.
pub(crate) fn tcgetpgrp(fd: RawFd) -> Option<i32> {
    // SAFETY: tcgetpgrp on an open descriptor.
    let pgrp = unsafe { libc::tcgetpgrp(fd) };
    (pgrp > 0).then_some(pgrp)
}

/// Block until child `pid` has exited and return its exit code (128 + the
/// signal for a signal death), without reaping it: `WNOWAIT` leaves it a
/// zombie, which keeps its pid, and so its process group id, from being
/// reused until the caller reaps it.
pub(crate) fn wait_exited(pid: i32) -> io::Result<i64> {
    loop {
        // SAFETY: an all-zero siginfo_t is a valid out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid on our own child with a valid out-pointer.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: waitid filled `info` for a child state change.
        let status = i64::from(unsafe { info.si_status() });
        match info.si_code {
            libc::CLD_EXITED => return Ok(status),
            libc::CLD_KILLED | libc::CLD_DUMPED => return Ok(128 + status),
            // Darwin can report a stop despite WEXITED (Go issue 19314).
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Block SIGINT, SIGTERM and SIGHUP in the calling thread (and in every
/// thread it starts later), so [`wait_exit_signal`] can take them. Children
/// unblock everything in `child_setup`.
pub(crate) fn block_exit_signals() -> io::Result<libc::sigset_t> {
    // SAFETY: sigset operations on a local set, then this thread's mask.
    unsafe {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(set.as_mut_ptr());
        let mut set = set.assume_init();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        Ok(set)
    }
}

/// Undo [`block_exit_signals`] in the calling thread.
pub(crate) fn unblock_signals(set: &libc::sigset_t) {
    // SAFETY: a valid set; changes only this thread's mask.
    unsafe {
        libc::pthread_sigmask(libc::SIG_UNBLOCK, set, ptr::null_mut());
    }
}

/// Wait for one of the signals in `set` (blocked by [`block_exit_signals`]).
pub(crate) fn wait_exit_signal(set: &libc::sigset_t) -> libc::c_int {
    loop {
        let mut signal: libc::c_int = 0;
        // SAFETY: a valid set and out-pointer.
        if unsafe { libc::sigwait(set, &mut signal) } == 0 {
            return signal;
        }
    }
}
