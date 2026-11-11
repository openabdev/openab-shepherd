//! A pane's process tree (ADR §11): every process Shepherd has seen descend
//! from the pane's leader, remembered by pid and start time, so a member
//! that left the pane's group or session, or was reparented to pid 1, can
//! still be ended, and a reused pid is never signalled.
//!
//! Members are found by parent pid from the leader and from every member
//! still alive, and by membership of the leader's process group and session
//! (on macOS: the processes whose controlling terminal is the pane's). The
//! reader thread refreshes the tree every [`super::TREE_POLL`] while the
//! leader runs, and the stop path refreshes it again before signalling.

use std::collections::HashMap;

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
}

#[cfg(target_os = "linux")]
mod os {
    use super::Start;
    use std::collections::HashMap;

    struct Stat {
        ppid: i32,
        pgrp: i32,
        session: i32,
        start: Start,
    }

    /// `/proc/<pid>/stat`, or None when it is gone or a zombie.
    fn stat(pid: i32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may hold spaces and parentheses: fields start
        // after the last ')'.
        let rest = &text[text.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // fields[0] is field 3 of proc(5): state, ppid, pgrp, session, ...,
        // starttime (field 22).
        if matches!(fields.first(), Some(&"Z") | Some(&"X") | None) {
            return None;
        }
        Some(Stat {
            ppid: fields.get(1)?.parse().ok()?,
            pgrp: fields.get(2)?.parse().ok()?,
            session: fields.get(3)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
        })
    }

    pub(super) fn start_time(pid: i32) -> Option<Start> {
        stat(pid).map(|s| s.start)
    }

    /// One read of every process in `/proc`.
    pub(super) struct Snapshot {
        procs: HashMap<i32, Stat>,
    }

    impl Snapshot {
        pub(super) fn take(_leader: i32, _tty: &mut Option<u32>) -> Self {
            let procs = std::fs::read_dir("/proc")
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .filter_map(|pid| Some((pid, stat(pid)?)))
                .collect();
            Self { procs }
        }

        pub(super) fn start(&self, pid: i32) -> Option<Start> {
            self.procs.get(&pid).map(|s| s.start)
        }

        pub(super) fn children(&self, pid: i32) -> Vec<i32> {
            self.procs
                .iter()
                .filter(|(_, s)| s.ppid == pid)
                .map(|(&p, _)| p)
                .collect()
        }

        /// The leader's group and session.
        pub(super) fn peers(&self, leader: i32) -> Vec<i32> {
            self.procs
                .iter()
                .filter(|(_, s)| s.pgrp == leader || s.session == leader)
                .map(|(&p, _)| p)
                .collect()
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

    fn thread_sleep(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
