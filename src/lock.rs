use crate::domain::Limits;
use crate::state::{PathDiagnostic, StateError, StateStore, StorageIdentity};
use std::error::Error;
use std::ffi::CString;
use std::fmt::{self, Display, Formatter};
use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockOperation {
    Inspect,
    Open,
    Acquire,
}

pub enum LockError {
    UnsafePath {
        path: PathDiagnostic,
    },
    Io {
        operation: LockOperation,
        source: io::Error,
    },
    Contended {
        wait: Duration,
    },
    State(StateError),
}

impl Display for LockError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath { path } => {
                write!(formatter, "unsafe lock path: {path}")
            }
            Self::Io { operation, source } => {
                write!(formatter, "lock {operation:?} failed: {source}")
            }
            Self::Contended { wait } => write!(
                formatter,
                "repository lock remained contended for {} ms",
                wait.as_millis()
            ),
            Self::State(error) => Display::fmt(error, formatter),
        }
    }
}

impl fmt::Debug for LockError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for LockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::State(error) => Some(error),
            _ => None,
        }
    }
}

pub struct RepositoryLock {
    _file: File,
    path: PathBuf,
    storage_identity: StorageIdentity,
    lock_device: u64,
    lock_inode: u64,
}

impl fmt::Debug for RepositoryLock {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryLock")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RepositoryLock {
    pub fn acquire(store: &StateStore, limits: Limits) -> Result<Self, LockError> {
        store.validate_namespace().map_err(LockError::State)?;
        let path = store.state_directory_path().join("lock");
        let name = CString::new("lock").expect("static lock name has no NUL");
        // SAFETY: the retained state-directory descriptor and static name are valid.
        let fd = unsafe {
            libc::openat(
                store.state_directory_handle().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::ELOOP) {
                return Err(LockError::UnsafePath {
                    path: PathDiagnostic::new(&path),
                });
            }
            return Err(LockError::Io {
                operation: LockOperation::Open,
                source,
            });
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|source| LockError::Io {
            operation: LockOperation::Inspect,
            source,
        })?;
        // SAFETY: geteuid has no preconditions and returns the process credential.
        let effective_uid = unsafe { libc::geteuid() };
        let unsafe_metadata = !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != effective_uid
            || metadata.mode() & 0o077 != 0;
        if unsafe_metadata {
            return Err(LockError::UnsafePath {
                path: PathDiagnostic::new(&path),
            });
        }

        let wait = limits.lock_wait();
        let deadline = Instant::now() + wait;
        let mut first_attempt = true;
        loop {
            if !first_attempt && Instant::now() >= deadline {
                return Err(LockError::Contended { wait });
            }
            first_attempt = false;
            match file.try_lock() {
                Ok(()) => {
                    store.validate_namespace().map_err(LockError::State)?;
                    let metadata = file.metadata().map_err(|source| LockError::Io {
                        operation: LockOperation::Inspect,
                        source,
                    })?;
                    let lock = Self {
                        _file: file,
                        path,
                        storage_identity: store.storage_identity(),
                        lock_device: metadata.dev(),
                        lock_inode: metadata.ino(),
                    };
                    if !lock.validates_store(store) {
                        return Err(LockError::UnsafePath {
                            path: PathDiagnostic::new(&lock.path),
                        });
                    }
                    return Ok(lock);
                }
                Err(TryLockError::WouldBlock) => {
                    thread::sleep(
                        RETRY_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Err(TryLockError::Error(source)) => {
                    return Err(LockError::Io {
                        operation: LockOperation::Acquire,
                        source,
                    });
                }
            }
        }
    }

    pub(crate) fn validates_store(&self, store: &StateStore) -> bool {
        if self.storage_identity != store.storage_identity() {
            return false;
        }
        let name = CString::new("lock").expect("static lock name has no NUL");
        // SAFETY: the retained state-directory descriptor and static name are valid.
        let fd = unsafe {
            libc::openat(
                store.state_directory_handle().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return false;
        }
        // SAFETY: openat returned a new owned descriptor.
        let current = unsafe { File::from_raw_fd(fd) };
        current.metadata().is_ok_and(|metadata| {
            // SAFETY: geteuid has no preconditions.
            let effective_uid = unsafe { libc::geteuid() };
            metadata.is_file()
                && metadata.dev() == self.lock_device
                && metadata.ino() == self.lock_inode
                && metadata.nlink() == 1
                && metadata.uid() == effective_uid
                && metadata.mode() & 0o077 == 0
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
