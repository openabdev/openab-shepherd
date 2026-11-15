//! The front side of `hold` (issue #76): what `openab-shepherd front` does
//! with the two link ends the holder gave it.
//!
//! A front under a holder never ends panes. Its signals and its panic hook
//! exit without `end_all`, and the link closing (either socket) exits it with
//! status 1; the holder, which owns the panes, decides what happens next.

use super::client::{ConnectError, HolderClient};
use super::pty;
use crate::native::{NativeBackend, NativeConfig};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// The front's exit status for a holder that speaks another protocol version
/// (`EX_CONFIG`).
pub const EXIT_VERSION_MISMATCH: i32 = 78;

/// The front's exit status when the link to the holder closes.
pub const EXIT_LINK_CLOSED: i32 = 1;

/// Test hook (always compiled in, see the ADR notes): the protocol version
/// the front speaks instead of the real one. An integer, or `@path` naming a
/// file that holds one, read each time a front starts, so a test or an
/// operator can change it without restarting the holder (the environment of
/// a front is the holder's, frozen at its start). A missing or empty file
/// means the real version.
pub const TEST_VERSION_ENV: &str = "OPENAB_SHEPHERD_TEST_HOLDER_VERSION";

/// Why a front could not attach to its holder.
#[derive(Debug)]
pub enum FrontError {
    /// The holder speaks another protocol version.
    VersionMismatch {
        holder: u32,
        front: u32,
    },
    Other(String),
}

impl std::fmt::Display for FrontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { holder, front } => {
                write!(f, "holder protocol {holder}, front {front}")
            }
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for FrontError {}

/// The protocol version this front speaks: the test override when set, else
/// the real one. `Err` names a bad override.
pub fn front_version(override_value: Option<&str>) -> Result<u32, String> {
    let real = super::wire::HOLDER_VERSION;
    let Some(value) = override_value else {
        return Ok(real);
    };
    let parse = |text: &str| {
        text.trim()
            .parse::<u32>()
            .map_err(|_| format!("{TEST_VERSION_ENV}: {text:?} is not a version number"))
    };
    match value.strip_prefix('@') {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(real),
            Ok(text) => parse(&text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(real),
            Err(err) => Err(format!("{TEST_VERSION_ENV}: cannot read {path}: {err}")),
        },
        None => parse(value),
    }
}

/// Watches the link to the holder.
#[derive(Clone)]
pub struct LinkWatch {
    client: Arc<HolderClient>,
}

impl LinkWatch {
    /// Whether the link has closed (either socket).
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Block until the link closes.
    pub fn wait_closed(&self) {
        while !self.client.is_closed() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// A front attached to its holder, with the panes it adopted.
pub struct FrontLink {
    pub backend: NativeBackend,
    pub watch: LinkWatch,
    /// The panes adopted from the holder, by id.
    pub adopted: Vec<String>,
    /// Of those, the ones rebuilt from a wrapped tail (they refuse writes
    /// until they repaint).
    pub replayed: Vec<String>,
}

/// Take one of the two link ends the holder passed by number.
///
/// # Safety
/// `fd` must be an open descriptor this process owns and nothing else uses.
unsafe fn take_stream(fd: RawFd, what: &str) -> Result<UnixStream, FrontError> {
    // SAFETY: only a probe of the descriptor flags.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(FrontError::Other(format!(
            "the {what} descriptor {fd} is not open: {}",
            io::Error::last_os_error()
        )));
    }
    // Not for any child this process starts later.
    // SAFETY: setting close-on-exec on an open descriptor.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    // SAFETY: the caller owns `fd`.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Greet the holder over the two descriptors, speak protocol `version`, and
/// adopt every pane it holds. The panes stay when this process goes away.
///
/// # Safety
/// `ops_fd` and `events_fd` must be open descriptors this process owns and
/// nothing else uses (the holder's child ends of the two socketpairs).
pub unsafe fn connect_front(
    ops_fd: RawFd,
    events_fd: RawFd,
    version: u32,
) -> Result<FrontLink, FrontError> {
    // SAFETY: forwarded from the caller.
    let (ops, events) = unsafe {
        (
            take_stream(ops_fd, "ops")?,
            take_stream(events_fd, "events")?,
        )
    };
    let client = HolderClient::connect_as(ops, events, version).map_err(|err| match err {
        ConnectError::VersionMismatch { holder, front } => {
            FrontError::VersionMismatch { holder, front }
        }
        other => FrontError::Other(format!("cannot reach the holder: {other}")),
    })?;
    let client = Arc::new(client);
    let backend = NativeBackend::over_holder(NativeConfig::default(), Arc::clone(&client));
    backend.leave_panes_on_drop();
    let adoption = backend.adopt().map_err(|err| {
        FrontError::Other(format!("cannot adopt the holder's panes: {}", err.detail))
    })?;
    Ok(FrontLink {
        backend,
        watch: LinkWatch { client },
        adopted: adoption.adopted,
        replayed: adoption.replayed,
    })
}

/// On SIGINT, SIGTERM or SIGHUP: exit with 128 + the signal and leave every
/// pane to the holder. Call before starting any other thread.
pub fn exit_on_signals_leaving_panes() -> io::Result<()> {
    let set = pty::block_exit_signals()?;
    let started = thread::Builder::new()
        .name("front-signals".to_string())
        .spawn(move || {
            let signal = pty::wait_exit_signal(&set);
            eprintln!(
                "openab-shepherd: front: signal {signal}: exiting, panes stay with the holder"
            );
            std::process::exit(128 + signal);
        });
    if let Err(err) = started {
        pty::unblock_signals(&set);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_override_is_a_number_or_a_file_with_one() {
        let real = super::super::wire::HOLDER_VERSION;
        assert_eq!(front_version(None), Ok(real));
        assert_eq!(front_version(Some("7")), Ok(7));
        assert_eq!(front_version(Some(" 8 ")), Ok(8));
        assert!(front_version(Some("abc")).is_err());
        assert!(front_version(Some("-1")).is_err());

        let dir = crate::testutil::TempDir::new("front-version");
        let file = dir.path().join("v");
        let hook = format!("@{}", file.display());
        // No file: the real version. An empty file too.
        assert_eq!(front_version(Some(&hook)), Ok(real));
        std::fs::write(&file, "").unwrap();
        assert_eq!(front_version(Some(&hook)), Ok(real));
        std::fs::write(&file, "999\n").unwrap();
        assert_eq!(front_version(Some(&hook)), Ok(999));
        std::fs::write(&file, "junk").unwrap();
        assert!(front_version(Some(&hook)).is_err());
    }

    #[test]
    fn a_descriptor_that_is_not_open_is_refused_not_adopted() {
        // SAFETY: probing a descriptor number nothing in this test opens.
        let err = unsafe { connect_front(1000, 1001, 1) }.err().unwrap();
        assert!(matches!(err, FrontError::Other(_)), "{err}");
    }
}
