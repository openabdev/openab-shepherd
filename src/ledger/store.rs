//! Where the log bytes live.
//!
//! [`Store`] is the seam: the ledger only needs to load all bytes, append one
//! record durably and cut the log back. [`FileStore`] is the one backend.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Append-only storage for the log.
pub trait Store {
    /// All bytes of the log.
    fn load(&mut self) -> io::Result<Vec<u8>>;

    /// Append one whole record. `Ok` means it is durable: written and synced.
    /// On `Err` the record may or may not be on disk; the caller treats it as
    /// not written and stops appending until the log is reopened.
    fn append(&mut self, record: &[u8]) -> io::Result<()>;

    /// Cut the log back to `len` bytes, durably. Used once at open, to drop a
    /// torn final record.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// The log in one file: mode 0600, opened for append, held under an exclusive
/// advisory lock so a second writer cannot interleave records. A symlink at
/// the path is refused. The mode applies when the file is created: a file that
/// already exists keeps its mode. Local filesystem and cooperating writers
/// only, because `flock` does not exclude writers on some network filesystems.
pub struct FileStore {
    file: File,
    /// Length of the last durable record boundary.
    len: u64,
    /// Set when a failure leaves the file in an unknown state.
    poisoned: bool,
}

impl FileStore {
    /// Open or create the log at `path`. Fails if another process holds it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Self::from_file(file, !existed, path)
    }

    fn from_file(file: File, created: bool, path: &Path) -> io::Result<Self> {
        // SAFETY: `flock` on a file descriptor this process owns.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            return Err(if err.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "the ledger log is open in another process",
                )
            } else {
                err
            });
        }
        if created {
            // A new file is only durable once its directory entry is.
            let dir = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            File::open(dir)?.sync_all()?;
        }
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            poisoned: false,
        })
    }

    fn poisoned_error() -> io::Error {
        io::Error::other("the ledger log failed to sync and must be reopened")
    }
}

impl Store for FileStore {
    fn load(&mut self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn append(&mut self, record: &[u8]) -> io::Result<()> {
        if self.poisoned {
            return Err(Self::poisoned_error());
        }
        if let Err(e) = self.file.write_all(record) {
            // A partial write would sit in front of the next record. Cut it
            // off; if that fails too, the file is in an unknown state.
            if self.file.set_len(self.len).is_err() {
                self.poisoned = true;
            }
            return Err(e);
        }
        if let Err(e) = self.file.sync_data() {
            // After a failed fsync the kernel may have dropped the data, so
            // nothing about the file is certain. Refuse everything further.
            self.poisoned = true;
            return Err(e);
        }
        self.len += record.len() as u64;
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if self.poisoned {
            return Err(Self::poisoned_error());
        }
        self.file.set_len(len)?;
        if let Err(e) = self.file.sync_data() {
            self.poisoned = true;
            return Err(e);
        }
        self.len = len;
        Ok(())
    }
}

#[cfg(test)]
impl FileStore {
    /// A store over a file the caller opened, for failure tests.
    pub(crate) fn over(file: File) -> Self {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Self {
            file,
            len,
            poisoned: false,
        }
    }
}
