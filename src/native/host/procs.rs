//! A pane's process tree (ADR §11): every process Shepherd has seen descend
//! from the pane's leader, remembered by pid and start time, so a member
//! that left the pane's group or session, or was reparented to pid 1, can
//! still be ended, and a reused pid is never signalled.
//!
//! Members are found by parent pid from the leader and from every member
//! still alive, and by membership of the leader's process group and session
//! (on macOS: the processes whose controlling terminal is the pane's). The
//! reader thread refreshes the tree every `TREE_POLL` while the
//! leader runs, and the stop path refreshes it again before signalling.
//!
//! On Linux every refresh needs the whole process table. The reader threads
//! of all the holder's panes share one [`Shared`] snapshot of it, at most
//! [`SNAPSHOT_MAX_AGE`] old, so a tick costs one scan, not one per pane. The
//! stop path never uses it: it takes its own fresh scan ([`Tree::refresh`]),
//! and a signal is sent only after the pid and its start time are read again.
//! `Shared`'s lock is a leaf: nothing else is taken while it is held.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The oldest a shared snapshot may be when a pane's reader uses it.
pub(crate) const SNAPSHOT_MAX_AGE: Duration = Duration::from_millis(250);

/// When a process started, in platform units. With the pid it names one
/// process: a pid that is reused gets a later start time.
pub(crate) type Start = u64;

/// The pane's known descendants (the leader itself is not a member).
#[derive(Default)]
pub(crate) struct Tree {
    members: HashMap<i32, Start>,
    /// macOS: the controlling terminal of the leader's session.
    tty: Option<u32>,
}

impl Tree {
    /// Add the leader's current descendants and drop members that are gone
    /// (or whose pid now names another process). Call only while the
    /// leader's pid is reserved (it runs, or is an unreaped zombie), because
    /// the group and session ids are the leader's pid. Returns how many
    /// processes were new.
    pub(crate) fn refresh(&mut self, leader: i32) -> usize {
        let snap = os::Snapshot::take(leader, &mut self.tty);
        self.apply(leader, &snap)
    }

    /// Like [`Tree::refresh`], from the holder-wide snapshot when it is
    /// recent enough and was taken at or after `not_before` (when the pane
    /// was spawned, so it cannot predate the leader). For the periodic
    /// look only: a stop must use `refresh`, because a snapshot up to
    /// `SNAPSHOT_MAX_AGE` old can miss a process forked since.
    pub(crate) fn refresh_shared(
        &mut self,
        leader: i32,
        shared: &Shared,
        not_before: Instant,
    ) -> usize {
        let snap = shared.0.snapshot(leader, &mut self.tty, not_before);
        self.apply(leader, &snap)
    }

    fn apply(&mut self, leader: i32, snap: &os::Snapshot) -> usize {
        let mut known: HashMap<i32, Start> = self
            .members
            .iter()
            .filter(|(&pid, &start)| snap.start(pid) == Some(start))
            .map(|(&pid, &start)| (pid, start))
            .collect();
        let me = std::process::id() as i32;
        let mut frontier: Vec<i32> = known.keys().copied().collect();
        frontier.push(leader);
        let add = |pid: i32, known: &mut HashMap<i32, Start>, frontier: &mut Vec<i32>| {
            if pid <= 1 || pid == me || pid == leader || known.contains_key(&pid) {
                return;
            }
            if let Some(start) = snap.start(pid) {
                known.insert(pid, start);
                frontier.push(pid);
            }
        };
        for pid in snap.peers(leader) {
            add(pid, &mut known, &mut frontier);
        }
        while let Some(parent) = frontier.pop() {
            for pid in snap.children(parent) {
                add(pid, &mut known, &mut frontier);
            }
        }
        let added = known
            .keys()
            .filter(|pid| !self.members.contains_key(pid))
            .count();
        self.members = known;
        added
    }

    /// Send `signal` to every member that is still the process recorded.
    pub(crate) fn signal_all(&self, signal: libc::c_int) {
        for (&pid, &start) in &self.members {
            signal_if_same(pid, start, signal);
        }
    }

    /// Whether any member is still running (zombies do not count).
    pub(crate) fn any_alive(&self) -> bool {
        self.members
            .iter()
            .any(|(&pid, &start)| os::start_time(pid) == Some(start))
    }
}

/// The process table as seen by one holder, shared by its panes' readers.
pub(crate) struct Shared(os::Shared);

impl Shared {
    pub(crate) fn new() -> Self {
        Self(os::Shared::new(SNAPSHOT_MAX_AGE))
    }

    /// Test hook: a different age limit.
    #[cfg(test)]
    pub(crate) fn with_max_age(max_age: Duration) -> Self {
        Self(os::Shared::new(max_age))
    }

    /// How many scans of the process table this has taken.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn scans(&self) -> usize {
        self.0.scans()
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// Signal `pid` only if it is still the process that started at `start`.
/// The window between the check and the kill is a few microseconds; a pid
/// is reused only after the pid space wraps, so a reuse inside it is not a
/// practical risk (ADR §11).
fn signal_if_same(pid: i32, start: Start, signal: libc::c_int) {
    if pid > 1 && os::start_time(pid) == Some(start) {
        // SAFETY: kill on a single positive pid, checked above.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::Start;
    use std::mem::{size_of, MaybeUninit};
    use std::time::{Duration, Instant};

    /// `PROC_TTY_ONLY` in `<sys/proc_info.h>`: processes whose session's
    /// controlling terminal is the given device.
    const PROC_TTY_ONLY: u32 = 3;
    const NODEV: u32 = u32::MAX;
    const MAX_PIDS: usize = 4096;

    fn bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: a buffer of exactly `size` bytes; the kernel fills it.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        // SAFETY: a full-size answer means the struct was written.
        (n == size).then(|| unsafe { info.assume_init() })
    }

    /// The process's start time in microseconds, or None when it is gone
    /// or a zombie (the kernel does not answer for zombies).
    pub(super) fn start_time(pid: i32) -> Option<Start> {
        let info = bsdinfo(pid)?;
        (info.pbi_pid as i32 == pid)
            .then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    /// Pids from a `proc_list*pids`-style call that returns a count.
    fn pids(list: impl Fn(*mut libc::c_void, libc::c_int) -> libc::c_int) -> Vec<i32> {
        let mut buf = vec![0i32; MAX_PIDS];
        let bytes = (buf.len() * size_of::<i32>()) as libc::c_int;
        let n = list(buf.as_mut_ptr().cast(), bytes);
        buf.truncate(n.clamp(0, MAX_PIDS as libc::c_int) as usize);
        buf.retain(|&pid| pid > 0);
        buf
    }

    /// macOS asks the kernel per question; nothing is cached.
    pub(super) struct Snapshot {
        tty: Option<u32>,
    }

    impl Snapshot {
        pub(super) fn take(leader: i32, tty: &mut Option<u32>) -> Self {
            if tty.is_none() {
                *tty = bsdinfo(leader)
                    .map(|info| info.e_tdev)
                    .filter(|&dev| dev != NODEV && dev != 0);
            }
            Self { tty: *tty }
        }

        pub(super) fn start(&self, pid: i32) -> Option<Start> {
            start_time(pid)
        }

        pub(super) fn children(&self, pid: i32) -> Vec<i32> {
            // SAFETY: the buffer and its size come from `pids`.
            pids(|buf, size| unsafe { libc::proc_listchildpids(pid, buf, size) })
        }

        /// The leader's group, and the processes on the pane's terminal.
        pub(super) fn peers(&self, leader: i32) -> Vec<i32> {
            // SAFETY: the buffer and its size come from `pids`.
            let mut out = pids(|buf, size| unsafe { libc::proc_listpgrppids(leader, buf, size) });
            if let Some(dev) = self.tty {
                // proc_listpids returns bytes, not a count.
                // SAFETY: the buffer and its size come from `pids`.
                out.extend(pids(|buf, size| unsafe {
                    libc::proc_listpids(PROC_TTY_ONLY, dev, buf, size) / size_of::<i32>() as i32
                }));
            }
            out
        }
    }

    /// macOS asks the kernel per question and keeps no table, so there is
    /// nothing to share: each look is its own.
    pub(super) struct Shared;

    impl Shared {
        pub(super) fn new(_max_age: Duration) -> Self {
            Self
        }

        pub(super) fn snapshot(
            &self,
            leader: i32,
            tty: &mut Option<u32>,
            _not_before: Instant,
        ) -> Snapshot {
            Snapshot::take(leader, tty)
        }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::Start;
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::fs::File;
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    pub(super) struct Stat {
        pub(super) ppid: i32,
        pub(super) pgrp: i32,
        pub(super) session: i32,
        pub(super) start: Start,
    }

    fn int<T: std::str::FromStr>(token: &[u8]) -> Option<T> {
        std::str::from_utf8(token).ok()?.parse().ok()
    }

    /// The fields of one `/proc/<pid>/stat` line, or None for a zombie, a
    /// dead process, or a line that is cut short. Read in place: no
    /// allocation. Bytes, not text, so a command name that is not UTF-8
    /// does not hide the process.
    pub(super) fn parse_stat(line: &[u8]) -> Option<Stat> {
        // The command name may hold spaces and parentheses: fields start
        // after the last ')'.
        let rest = &line[line.iter().rposition(|&b| b == b')')? + 1..];
        let mut fields = rest
            .split(|b| b.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        // The first field is field 3 of proc(5): state, ppid, pgrp, session,
        // ..., starttime (field 22, 15 fields after session).
        if matches!(fields.next()?, b"Z" | b"X") {
            return None;
        }
        let ppid = int(fields.next()?)?;
        let pgrp = int(fields.next()?)?;
        let session = int(fields.next()?)?;
        let start = int(fields.nth(15)?)?;
        Some(Stat {
            ppid,
            pgrp,
            session,
            start,
        })
    }

    /// Reads `/proc/<pid>/stat` into a path and a buffer it keeps, so a scan
    /// of every process allocates once, not once per process.
    #[derive(Default)]
    pub(super) struct Reader {
        path: String,
        buf: Vec<u8>,
    }

    impl Reader {
        /// `/proc/<pid>/stat`, or None when it is gone or a zombie.
        pub(super) fn stat(&mut self, pid: i32) -> Option<Stat> {
            self.path.clear();
            write!(self.path, "/proc/{pid}/stat").ok()?;
            self.buf.clear();
            File::open(&self.path)
                .ok()?
                .read_to_end(&mut self.buf)
                .ok()?;
            parse_stat(&self.buf)
        }
    }

    pub(super) fn start_time(pid: i32) -> Option<Start> {
        Reader::default().stat(pid).map(|s| s.start)
    }

    /// One read of every process in `/proc`, with the lookups the tree needs
    /// indexed once: by parent, and by process group and session.
    pub(super) struct Snapshot {
        procs: HashMap<i32, Stat>,
        children: HashMap<i32, Vec<i32>>,
        /// Members of each process group and session, by its id.
        peers: HashMap<i32, Vec<i32>>,
    }

    impl Snapshot {
        pub(super) fn take(_leader: i32, _tty: &mut Option<u32>) -> Self {
            let pids = std::fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok());
            Self::of(pids)
        }

        /// The listed processes that can still be read: one that left
        /// between the listing and the read is skipped.
        pub(super) fn of(pids: impl IntoIterator<Item = i32>) -> Self {
            let mut reader = Reader::default();
            let procs: HashMap<i32, Stat> = pids
                .into_iter()
                .filter_map(|pid| Some((pid, reader.stat(pid)?)))
                .collect();
            let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
            let mut peers: HashMap<i32, Vec<i32>> = HashMap::new();
            for (&pid, s) in &procs {
                children.entry(s.ppid).or_default().push(pid);
                peers.entry(s.pgrp).or_default().push(pid);
                if s.session != s.pgrp {
                    peers.entry(s.session).or_default().push(pid);
                }
            }
            Self {
                procs,
                children,
                peers,
            }
        }

        pub(super) fn start(&self, pid: i32) -> Option<Start> {
            self.procs.get(&pid).map(|s| s.start)
        }

        pub(super) fn children(&self, pid: i32) -> Vec<i32> {
            self.children.get(&pid).cloned().unwrap_or_default()
        }

        /// The leader's group and session.
        pub(super) fn peers(&self, leader: i32) -> Vec<i32> {
            self.peers.get(&leader).cloned().unwrap_or_default()
        }
    }

    /// The newest scan and when it began. One scan serves every reader that
    /// asks within `max_age`; the lock is held across the scan, so readers
    /// that arrive meanwhile wait for it instead of each taking their own.
    pub(super) struct Shared {
        max_age: Duration,
        last: Mutex<Option<(Instant, Arc<Snapshot>)>>,
        scans: AtomicUsize,
    }

    impl Shared {
        pub(super) fn new(max_age: Duration) -> Self {
            Self {
                max_age,
                last: Mutex::new(None),
                scans: AtomicUsize::new(0),
            }
        }

        /// A scan that began at or after `not_before` and less than
        /// `max_age` ago, taking one if there is none.
        pub(super) fn snapshot(
            &self,
            leader: i32,
            tty: &mut Option<u32>,
            not_before: Instant,
        ) -> Arc<Snapshot> {
            let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((began, snap)) = &*last {
                if *began >= not_before && began.elapsed() < self.max_age {
                    return Arc::clone(snap);
                }
            }
            let began = Instant::now();
            let snap = Arc::new(Snapshot::take(leader, tty));
            self.scans.fetch_add(1, Ordering::Relaxed);
            *last = Some((began, Arc::clone(&snap)));
            snap
        }

        #[cfg(test)]
        pub(super) fn scans(&self) -> usize {
            self.scans.load(Ordering::Relaxed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A member whose pid now has another start time (a reused pid) is not
    /// signalled; the same pid with its real start time is.
    #[test]
    fn a_member_with_another_start_time_is_not_signalled() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let start = os::start_time(pid).expect("the child's start time");
        let mut tree = Tree::default();
        tree.members.insert(pid, start + 1);
        tree.signal_all(libc::SIGKILL);
        thread_sleep(200);
        assert!(
            child.try_wait().unwrap().is_none(),
            "a reused pid was signalled"
        );
        tree.members.insert(pid, start);
        tree.signal_all(libc::SIGKILL);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "the recorded member was not signalled"
            );
            thread_sleep(20);
        }
    }

    /// A process tree to look for: `sh` leads its own session and runs a
    /// sleep, a subshell with a sleep of its own, and a child that moved to
    /// another session (found by parent, not by group or session).
    /// Returns the leader and the pids the script reported.
    struct Fixture {
        leader: std::process::Child,
        members: Vec<i32>,
        dir: crate::testutil::TempDir,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            use std::os::unix::process::CommandExt;
            let dir = crate::testutil::TempDir::new(name);
            let file = dir.path().join("pids");
            let script = "sleep 60 & echo $! >>\"$F\"; \
                (sleep 60 & echo $! >>\"$F\"; wait) & echo $! >>\"$F\"; \
                perl -MPOSIX -e 'POSIX::setsid(); sleep 60' & echo $! >>\"$F\"; wait";
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", script])
                .env("F", &file)
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            // SAFETY: setsid between fork and exec, nothing else.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let leader = cmd.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut members: Vec<i32>;
            loop {
                members = std::fs::read_to_string(&file)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| l.trim().parse().ok())
                    .collect();
                // Four pids, all running.
                let ready = members.len() == 4
                    && members.iter().all(|&p| os::start_time(p).is_some())
                    // The perl has left the leader's session (it leads its own).
                    // SAFETY: getsid only reads a process's session id.
                    && members.iter().any(|&p| unsafe { libc::getsid(p) } == p);
                if ready {
                    break;
                }
                assert!(Instant::now() < deadline, "the fixture did not start");
                thread_sleep(20);
            }
            members.sort_unstable();
            Self {
                leader,
                members,
                dir,
            }
        }

        fn pid(&self) -> i32 {
            self.leader.id() as i32
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: kill on the fixture's own group.
            unsafe {
                libc::killpg(self.pid(), libc::SIGKILL);
            }
            for pid in &self.members {
                // SAFETY: kill on a pid the fixture started.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
            let _ = self.leader.wait();
            let _ = &self.dir;
        }
    }

    fn sorted(tree: &Tree) -> Vec<i32> {
        let mut pids: Vec<i32> = tree.members.keys().copied().collect();
        pids.sort_unstable();
        pids
    }

    /// `refresh` finds the leader's group, its session and a child that left
    /// both, and nothing else; a second refresh adds nothing.
    #[test]
    fn refresh_finds_exactly_the_fixture_tree() {
        let fx = Fixture::new("procs-fixture-fresh");
        let mut tree = Tree::default();
        assert_eq!(tree.refresh(fx.pid()), 4);
        assert_eq!(sorted(&tree), fx.members);
        assert_eq!(tree.refresh(fx.pid()), 0);
        assert_eq!(sorted(&tree), fx.members);
        assert!(tree.any_alive());
    }

    /// The shared snapshot finds the same members as a fresh scan.
    #[test]
    fn refresh_shared_finds_exactly_the_fixture_tree() {
        let fx = Fixture::new("procs-fixture-shared");
        let shared = Shared::new();
        let mut tree = Tree::default();
        assert_eq!(
            tree.refresh_shared(fx.pid(), &shared, Instant::now() - Duration::from_secs(60)),
            4
        );
        assert_eq!(sorted(&tree), fx.members);
    }

    /// A member that has gone is dropped by the next refresh.
    #[test]
    fn a_member_that_exited_is_dropped() {
        let fx = Fixture::new("procs-fixture-drop");
        let mut tree = Tree::default();
        tree.refresh(fx.pid());
        let gone = *fx.members.last().unwrap();
        // SAFETY: kill on a pid the fixture started.
        unsafe {
            libc::kill(gone, libc::SIGKILL);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while os::start_time(gone).is_some() {
            assert!(Instant::now() < deadline, "{gone} did not exit");
            thread_sleep(20);
        }
        tree.refresh(fx.pid());
        assert!(!tree.members.contains_key(&gone));
        // Whatever else is still running (a sleep whose parent was killed
        // is re-parented, but is still in the leader's session) stays.
        let alive: Vec<i32> = fx
            .members
            .iter()
            .copied()
            .filter(|&p| os::start_time(p).is_some())
            .collect();
        assert!(!alive.is_empty() && !alive.contains(&gone));
        assert_eq!(sorted(&tree), alive);
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use os::{parse_stat, Reader, Snapshot};

        /// A stat line with `comm` between the parentheses; the fields after
        /// it are state, ppid 10, pgrp 20, session 30, then filler up to
        /// starttime 4242 (field 22), then more fields.
        fn line(pid: i32, comm: &[u8], state: &str) -> Vec<u8> {
            let mut out = format!("{pid} (").into_bytes();
            out.extend_from_slice(comm);
            // fields 3..=21 are state, ppid, pgrp, session, then 15 more.
            out.extend_from_slice(format!(") {state} 10 20 30").as_bytes());
            for n in 0..15 {
                out.extend_from_slice(format!(" {}", 100 + n).as_bytes());
            }
            out.extend_from_slice(b" 4242 999 888 777\n");
            out
        }

        fn fields(bytes: &[u8]) -> Option<(i32, i32, i32, u64)> {
            parse_stat(bytes).map(|s| (s.ppid, s.pgrp, s.session, s.start))
        }

        #[test]
        fn a_plain_line_is_parsed() {
            assert_eq!(fields(&line(7, b"sleep", "S")), Some((10, 20, 30, 4242)));
        }

        #[test]
        fn a_comm_with_spaces_and_parentheses_is_parsed() {
            for comm in [
                &b"a b c"[..],
                b"(x)",
                b") R 1 2 3 (",
                b"((((",
                b"))))",
                b"",
                b" ",
            ] {
                assert_eq!(
                    fields(&line(7, comm, "R")),
                    Some((10, 20, 30, 4242)),
                    "comm {:?}",
                    String::from_utf8_lossy(comm)
                );
            }
        }

        #[test]
        fn a_very_long_comm_is_parsed() {
            let comm = vec![b'x'; 100_000];
            assert_eq!(fields(&line(7, &comm, "S")), Some((10, 20, 30, 4242)));
            let mut tricky = b") ".repeat(20_000);
            tricky.extend_from_slice(b"(");
            assert_eq!(fields(&line(7, &tricky, "S")), Some((10, 20, 30, 4242)));
        }

        /// The old text parser dropped such a process; ending it matters
        /// more than the name.
        #[test]
        fn a_comm_that_is_not_utf8_is_parsed() {
            assert_eq!(
                fields(&line(7, &[0xff, 0xfe, b'(', 0x80], "S")),
                Some((10, 20, 30, 4242))
            );
        }

        #[test]
        fn a_zombie_or_dead_process_is_not_found() {
            assert_eq!(fields(&line(7, b"z", "Z")), None);
            assert_eq!(fields(&line(7, b"x", "X")), None);
            assert!(fields(&line(7, b"x", "x")).is_some());
        }

        #[test]
        fn a_cut_or_malformed_line_is_not_found() {
            assert_eq!(fields(b""), None);
            assert_eq!(fields(b"7 (sleep"), None);
            assert_eq!(fields(b"7 (sleep)"), None);
            assert_eq!(fields(b"7 (sleep) S 10 20"), None);
            let full = line(7, b"sleep", "S");
            // Cut before starttime (field 22): no start, no process.
            let before_start = full.len() - " 4242 999 888 777\n".len();
            assert_eq!(fields(&full[..before_start]), None);
            assert_eq!(
                fields(b"7 (sleep) S x 20 30 1 2 3 4 5 6 7 8 9 10 11 12 13 4242"),
                None
            );
        }

        /// A real zombie is not in a snapshot; its parent is.
        #[test]
        fn a_real_zombie_is_not_in_a_snapshot() {
            let mut child = Command::new("/bin/true").spawn().unwrap();
            let pid = child.id() as i32;
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut reader = Reader::default();
            // Not waited for: the process stays a zombie once it has exited.
            while std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|t| !t.contains(") Z "))
                .unwrap_or(true)
            {
                assert!(Instant::now() < deadline, "no zombie appeared");
                thread_sleep(10);
            }
            assert!(reader.stat(pid).is_none());
            assert!(os::start_time(pid).is_none());
            let snap = Snapshot::take(0, &mut None);
            assert!(snap.start(pid).is_none());
            assert!(snap.start(std::process::id() as i32).is_some());
            child.wait().unwrap();
        }

        /// A process listed by readdir but gone at the read is skipped.
        #[test]
        fn a_process_that_vanished_between_listing_and_reading_is_skipped() {
            let me = std::process::id() as i32;
            let mut child = Command::new("/bin/true").spawn().unwrap();
            let gone = child.id() as i32;
            child.wait().unwrap();
            let snap = Snapshot::of([me, gone, i32::MAX, 0, -5]);
            assert!(snap.start(me).is_some());
            assert!(snap.start(gone).is_none());
            assert!(snap.start(i32::MAX).is_none());
            assert_eq!(Reader::default().stat(i32::MAX).map(|s| s.start), None);
        }

        /// The snapshot's indexes agree with the stat of each process.
        #[test]
        fn the_indexes_list_each_process_once_under_its_parent_group_and_session() {
            let fx = Fixture::new("procs-index");
            let snap = Snapshot::take(0, &mut None);
            let mut reader = Reader::default();
            for &pid in &fx.members {
                let s = reader.stat(pid).unwrap();
                let kids = snap.children(s.ppid);
                assert_eq!(kids.iter().filter(|&&k| k == pid).count(), 1);
                for id in [s.pgrp, s.session] {
                    let peers = snap.peers(id);
                    assert_eq!(
                        peers.iter().filter(|&&k| k == pid).count(),
                        1,
                        "{pid} in {id}"
                    );
                }
            }
            assert!(snap.children(i32::MAX).is_empty());
            assert!(snap.peers(i32::MAX).is_empty());
        }

        /// A snapshot within the age limit is reused; an older one is not.
        #[test]
        fn a_shared_snapshot_is_reused_within_the_age_limit_only() {
            let long = Shared::with_max_age(Duration::from_secs(3600));
            let epoch = Instant::now() - Duration::from_secs(60);
            let mut tty = None;
            let a = long.0.snapshot(1, &mut tty, epoch);
            let b = long.0.snapshot(1, &mut tty, epoch);
            assert!(std::sync::Arc::ptr_eq(&a, &b));
            assert_eq!(long.scans(), 1);

            let short = Shared::with_max_age(Duration::from_millis(60));
            let a = short.0.snapshot(1, &mut tty, epoch);
            thread_sleep(100);
            let b = short.0.snapshot(1, &mut tty, epoch);
            assert!(!std::sync::Arc::ptr_eq(&a, &b));
            assert_eq!(short.scans(), 2);

            let none = Shared::with_max_age(Duration::ZERO);
            none.0.snapshot(1, &mut tty, epoch);
            none.0.snapshot(1, &mut tty, epoch);
            assert_eq!(none.scans(), 2);
        }

        /// A snapshot that began before the pane did is never used for it.
        #[test]
        fn a_shared_snapshot_older_than_the_pane_is_not_used() {
            let shared = Shared::with_max_age(Duration::from_secs(3600));
            let mut tty = None;
            let epoch = Instant::now() - Duration::from_secs(60);
            shared.0.snapshot(1, &mut tty, epoch);
            thread_sleep(5);
            shared.0.snapshot(1, &mut tty, Instant::now());
            assert_eq!(shared.scans(), 2);
        }

        /// Panes that poll together cost one scan: 8 trees, one snapshot.
        #[test]
        fn eight_trees_refreshed_together_take_one_scan() {
            let shared = Shared::with_max_age(Duration::from_secs(3600));
            let epoch = Instant::now() - Duration::from_secs(60);
            let me = std::process::id() as i32;
            for _ in 0..8 {
                Tree::default().refresh_shared(me, &shared, epoch);
            }
            assert_eq!(shared.scans(), 1);
        }

        /// On the fixture the snapshot answers with the known tree: three
        /// children of the leader (one in another session), and the leader
        /// and three more in its group and session (the grandchild sleep,
        /// but not the perl).
        #[test]
        fn the_snapshot_answers_for_the_fixture_with_the_known_members() {
            let fx = Fixture::new("procs-known");
            let snap = Snapshot::take(0, &mut None);
            let mut reader = Reader::default();
            let mut kids = Vec::new();
            // The leader leads its own group and session.
            let mut group = vec![fx.pid()];
            for &pid in &fx.members {
                let s = reader.stat(pid).unwrap();
                assert_eq!(snap.start(pid), Some(s.start));
                if s.ppid == fx.pid() {
                    kids.push(pid);
                }
                if s.pgrp == fx.pid() || s.session == fx.pid() {
                    group.push(pid);
                }
            }
            assert_eq!(kids.len(), 3, "{kids:?}");
            assert_eq!(group.len(), 4, "{group:?}");
            let sorted_set = |mut v: Vec<i32>| {
                v.sort_unstable();
                v
            };
            assert_eq!(sorted_set(snap.children(fx.pid())), sorted_set(kids));
            assert_eq!(sorted_set(snap.peers(fx.pid())), sorted_set(group));
        }
    }

    fn thread_sleep(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
