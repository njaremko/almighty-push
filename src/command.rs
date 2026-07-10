use crate::domain::Limits;
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{CString, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const ARGUMENT_COUNT_MAX: usize = 512;
const ENVIRONMENT_COUNT_MAX: usize = 64;
const COMMAND_BYTES_MAX: usize = 4 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(1);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const READ_CHUNK_BYTES: usize = 8192;
const COMMAND_SESSION_COUNT_MAX: usize = 16;
static COMMAND_SESSION_COUNT: AtomicUsize = AtomicUsize::new(0);

pub enum SpecError {
    EmptyProgram,
    ProgramNotAbsolute,
    InteriorNul,
    InvalidWorkingDirectory {
        path: PathBuf,
        source: io::Error,
    },
    ArgumentCount {
        count: usize,
    },
    EnvironmentCount {
        count: usize,
    },
    InvalidEnvironment,
    ArgumentLimit {
        argument_bytes: usize,
        limit_bytes: usize,
    },
    EnvironmentLimit {
        environment_bytes: usize,
        limit_bytes: usize,
    },
    StdinLimit {
        stdin_bytes: usize,
        limit_bytes: usize,
    },
}

impl Display for SpecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProgram => formatter.write_str("command program is empty"),
            Self::ProgramNotAbsolute => formatter.write_str("command program is not absolute"),
            Self::InteriorNul => formatter.write_str("command input contains an interior NUL"),
            Self::InvalidWorkingDirectory { path, source } => write!(
                formatter,
                "command working directory is invalid: {}: {source}",
                path.display()
            ),
            Self::ArgumentCount { count } => write!(
                formatter,
                "command has {count} arguments, exceeding the compiled bound"
            ),
            Self::EnvironmentCount { count } => write!(
                formatter,
                "command has {count} environment entries, exceeding the compiled bound"
            ),
            Self::InvalidEnvironment => {
                formatter.write_str("command environment has an invalid or duplicate key")
            }
            Self::ArgumentLimit {
                argument_bytes,
                limit_bytes,
            } => write!(
                formatter,
                "command program and arguments use {argument_bytes} bytes, exceeding {limit_bytes}"
            ),
            Self::EnvironmentLimit {
                environment_bytes,
                limit_bytes,
            } => write!(
                formatter,
                "command environment uses {environment_bytes} bytes, exceeding {limit_bytes}"
            ),
            Self::StdinLimit {
                stdin_bytes,
                limit_bytes,
            } => write!(
                formatter,
                "command stdin uses {stdin_bytes} bytes, exceeding {limit_bytes}"
            ),
        }
    }
}

impl fmt::Debug for SpecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for SpecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidWorkingDirectory { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub struct CommandSpec {
    program: OsString,
    args: Box<[OsString]>,
    cwd: PathBuf,
    cwd_handle: File,
    stdin: Box<[u8]>,
    environment: Box<[(OsString, OsString)]>,
    timeout: Duration,
    output_limit_bytes: usize,
}

impl CommandSpec {
    pub fn new(
        program: OsString,
        args: Box<[OsString]>,
        cwd: PathBuf,
        stdin: Box<[u8]>,
        environment: Box<[(OsString, OsString)]>,
        limits: Limits,
    ) -> Result<Self, SpecError> {
        validate_program(&program)?;
        validate_arguments(&program, &args)?;
        validate_environment(&environment)?;
        if stdin.len() > COMMAND_BYTES_MAX {
            return Err(SpecError::StdinLimit {
                stdin_bytes: stdin.len(),
                limit_bytes: COMMAND_BYTES_MAX,
            });
        }
        let cwd_handle = open_directory_no_follow(&cwd).map_err(|source| {
            SpecError::InvalidWorkingDirectory {
                path: cwd.clone(),
                source,
            }
        })?;
        Ok(Self {
            program,
            args,
            cwd,
            cwd_handle,
            stdin,
            environment,
            timeout: limits.command_timeout(),
            output_limit_bytes: limits.command_output_bytes_max(),
        })
    }

    pub fn program(&self) -> &std::ffi::OsStr {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn stdin(&self) -> &[u8] {
        &self.stdin
    }

    pub fn environment_count(&self) -> usize {
        self.environment.len()
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn output_limit_bytes(&self) -> usize {
        self.output_limit_bytes
    }
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("argument_count", &self.args.len())
            .field("environment_count", &self.environment.len())
            .field("stdin_bytes", &self.stdin.len())
            .field("timeout", &self.timeout)
            .field("output_limit_bytes", &self.output_limit_bytes)
            .finish_non_exhaustive()
    }
}

fn validate_program(program: &OsString) -> Result<(), SpecError> {
    if program.is_empty() {
        return Err(SpecError::EmptyProgram);
    }
    if !Path::new(program).is_absolute() {
        return Err(SpecError::ProgramNotAbsolute);
    }
    if contains_nul(program) {
        return Err(SpecError::InteriorNul);
    }
    Ok(())
}

fn validate_arguments(program: &OsString, args: &[OsString]) -> Result<(), SpecError> {
    if args.len() > ARGUMENT_COUNT_MAX {
        return Err(SpecError::ArgumentCount { count: args.len() });
    }
    if args.iter().any(contains_nul) {
        return Err(SpecError::InteriorNul);
    }
    let argument_bytes = os_vector_bytes(program, args).ok_or(SpecError::ArgumentLimit {
        argument_bytes: usize::MAX,
        limit_bytes: COMMAND_BYTES_MAX,
    })?;
    if argument_bytes > COMMAND_BYTES_MAX {
        return Err(SpecError::ArgumentLimit {
            argument_bytes,
            limit_bytes: COMMAND_BYTES_MAX,
        });
    }
    Ok(())
}

fn validate_environment(environment: &[(OsString, OsString)]) -> Result<(), SpecError> {
    if environment.len() > ENVIRONMENT_COUNT_MAX {
        return Err(SpecError::EnvironmentCount {
            count: environment.len(),
        });
    }
    let mut keys = BTreeSet::new();
    let mut bytes = (environment.len() + 1)
        .checked_mul(std::mem::size_of::<usize>())
        .ok_or(SpecError::EnvironmentLimit {
            environment_bytes: usize::MAX,
            limit_bytes: COMMAND_BYTES_MAX,
        })?;
    for (key, value) in environment {
        if contains_nul(key) || contains_nul(value) {
            return Err(SpecError::InteriorNul);
        }
        let key_bytes = key.as_os_str().as_bytes();
        if key_bytes.is_empty() || key_bytes.contains(&b'=') || !keys.insert(key) {
            return Err(SpecError::InvalidEnvironment);
        }
        bytes = bytes
            .checked_add(key_bytes.len())
            .and_then(|count| count.checked_add(value.as_os_str().as_bytes().len()))
            .and_then(|count| count.checked_add(2))
            .ok_or(SpecError::EnvironmentLimit {
                environment_bytes: usize::MAX,
                limit_bytes: COMMAND_BYTES_MAX,
            })?;
    }
    if bytes > COMMAND_BYTES_MAX {
        return Err(SpecError::EnvironmentLimit {
            environment_bytes: bytes,
            limit_bytes: COMMAND_BYTES_MAX,
        });
    }
    Ok(())
}

fn contains_nul(value: &OsString) -> bool {
    value.as_os_str().as_bytes().contains(&0)
}

fn os_vector_bytes(program: &OsString, args: &[OsString]) -> Option<usize> {
    let mut bytes = (args.len() + 2).checked_mul(std::mem::size_of::<usize>())?;
    bytes = bytes.checked_add(program.as_os_str().as_bytes().len())?;
    bytes = bytes.checked_add(1)?;
    for arg in args {
        bytes = bytes.checked_add(arg.as_os_str().as_bytes().len())?;
        bytes = bytes.checked_add(1)?;
    }
    Some(bytes)
}

fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working directory is not absolute",
        ));
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "working directory is not canonical",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL")
        })?;
        // SAFETY: both descriptors and the NUL-terminated component are valid;
        // O_NOFOLLOW and directory-relative traversal bind each opened identity.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoOperation {
    AnchorSpawn,
    Nonblocking,
    StdinWrite,
    StdoutRead,
    StderrRead,
    Wait,
    Kill,
    Drain,
}

pub enum CommandError {
    Busy,
    Spawn {
        program: OsString,
        source: io::Error,
    },
    Timeout {
        timeout: Duration,
    },
    OutputLimit {
        limit_bytes: usize,
    },
    Exit {
        status_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Utf8 {
        stream: OutputStream,
        source: std::str::Utf8Error,
    },
    Io {
        operation: IoOperation,
        source: io::Error,
    },
    Cleanup {
        outcome: Box<CommandOutcome>,
        cleanup: Box<[CommandError]>,
    },
}

impl Display for CommandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("another command session is active"),
            Self::Spawn { source, .. } => write!(formatter, "failed to spawn command: {source}"),
            Self::Timeout { timeout } => write!(
                formatter,
                "command exceeded its {} ms execution timeout",
                timeout.as_millis()
            ),
            Self::OutputLimit { limit_bytes } => {
                write!(
                    formatter,
                    "command exceeded its {limit_bytes}-byte output bound"
                )
            }
            Self::Exit {
                status_code,
                stdout,
                stderr,
            } => write!(
                formatter,
                "command exited with {status_code:?}; {} stdout bytes; {} stderr bytes",
                stdout.len(),
                stderr.len()
            ),
            Self::Utf8 { stream, source } => write!(
                formatter,
                "command {stream:?} is not UTF-8 at byte {}",
                source.valid_up_to()
            ),
            Self::Io { operation, source } => {
                write!(formatter, "command {operation:?} failed: {source}")
            }
            Self::Cleanup { outcome, cleanup } => match outcome.as_ref() {
                CommandOutcome::Success(_) => write!(
                    formatter,
                    "command succeeded but cleanup had {} failures",
                    cleanup.len()
                ),
                CommandOutcome::Failure(primary) => write!(
                    formatter,
                    "{primary}; {} additional cleanup failures",
                    cleanup.len()
                ),
            },
        }
    }
}

impl fmt::Debug for CommandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for CommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::Io { source, .. } => Some(source),
            Self::Utf8 { source, .. } => Some(source),
            Self::Cleanup { outcome, .. } => match outcome.as_ref() {
                CommandOutcome::Success(_) => None,
                CommandOutcome::Failure(primary) => Some(primary),
            },
            _ => None,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub enum CommandOutcome {
    Success(CommandOutput),
    Failure(Box<CommandError>),
}

impl fmt::Debug for CommandOutcome {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success(output) => formatter.debug_tuple("Success").field(output).finish(),
            Self::Failure(error) => formatter.debug_tuple("Failure").field(error).finish(),
        }
    }
}

impl fmt::Debug for CommandOutput {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .finish()
    }
}

pub struct CommandRunner {
    active: AtomicBool,
}

/// Read-only command execution capability used by deterministic adapters.
///
/// Implementations must honor every bound and boundary encoded by `CommandSpec`.
pub trait CommandExecutor {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError>;
}

impl CommandRunner {
    /// Creates the process-wide child-process owner.
    ///
    /// # Safety
    ///
    /// On Linux the runner becomes a child subreaper. The caller must ensure all
    /// child creation in this process is coordinated by this boundary, so adopted
    /// children cannot belong to an unrelated subsystem.
    pub unsafe fn assume_process_child_authority() -> Self {
        Self {
            active: AtomicBool::new(false),
        }
    }

    pub fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let _permit = SessionPermit::acquire(&self.active)?;
        ensure_descendant_reaper()?;
        let deadline = Instant::now() + spec.timeout;
        let anchor = spawn_anchor().map_err(|source| CommandError::Io {
            operation: IoOperation::AnchorSpawn,
            source,
        })?;
        let pgid = checked_pid(anchor.id())?;
        assert!(pgid > 1);
        let mut session = OwnedSession::new(anchor, pgid);

        match spawn_command(spec, pgid) {
            Ok(child) => session.set_child(child),
            Err(error) => {
                let cleanup = session.cleanup();
                return Err(attach_cleanup(error, cleanup));
            }
        }
        let mut pipes = match Pipes::new(session.child_mut(), spec.output_limit_bytes) {
            Ok(pipes) => pipes,
            Err(error) => {
                let cleanup = session.cleanup();
                return Err(attach_cleanup(error, cleanup));
            }
        };

        let cause = match supervise(session.child_mut(), &mut pipes, deadline, &spec.stdin) {
            Ok(cause) => cause,
            Err(primary) => {
                let mut cleanup = session.cleanup();
                if let Err(error) = pipes.drain_until_eof(Instant::now() + CLEANUP_TIMEOUT) {
                    cleanup.push(error);
                }
                return Err(attach_cleanup(primary, cleanup));
            }
        };
        let cleanup = session.cleanup();
        let drain_deadline = Instant::now() + CLEANUP_TIMEOUT;
        let drain_result = pipes.drain_until_eof(drain_deadline);
        let mut cleanup_errors = cleanup;
        if let Err(error) = drain_result {
            cleanup_errors.push(error);
        }
        let cause = if pipes.output_exceeded {
            RunCause::OutputLimit
        } else {
            cause
        };
        let result = finish(cause, pipes, spec);
        if cleanup_errors.is_empty() {
            result
        } else {
            Err(attach_result_cleanup(result, cleanup_errors))
        }
    }
}

impl CommandExecutor for CommandRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        CommandRunner::run(self, spec)
    }
}

struct OwnedSession {
    anchor: Child,
    child: Option<Child>,
    pgid: i32,
    resolved: bool,
}

impl OwnedSession {
    fn new(anchor: Child, pgid: i32) -> Self {
        Self {
            anchor,
            child: None,
            pgid,
            resolved: false,
        }
    }

    fn set_child(&mut self, child: Child) {
        assert!(self.child.is_none());
        self.child = Some(child);
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child installed before use")
    }

    fn cleanup(&mut self) -> Vec<CommandError> {
        if self.resolved {
            return Vec::new();
        }
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut errors = Vec::new();
        let group_killed = match kill_group(self.pgid) {
            Ok(()) => true,
            Err(error) => {
                errors.push(error);
                false
            }
        };
        if let Some(child) = self.child.as_mut() {
            reap_owned([child, &mut self.anchor], deadline, &mut errors);
        } else {
            reap_owned([&mut self.anchor], deadline, &mut errors);
        }
        if !group_killed {
            std::process::abort();
        }
        reap_descendants(self.pgid, deadline, &mut errors);
        self.resolved = true;
        errors
    }
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        if !self.resolved {
            let _ = self.cleanup();
        }
    }
}

struct SessionPermit<'a> {
    runner_active: &'a AtomicBool,
}

impl<'a> SessionPermit<'a> {
    fn acquire(runner_active: &'a AtomicBool) -> Result<Self, CommandError> {
        runner_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| CommandError::Busy)?;
        let admitted = COMMAND_SESSION_COUNT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < COMMAND_SESSION_COUNT_MAX).then_some(count + 1)
            })
            .is_ok();
        if !admitted {
            runner_active.store(false, Ordering::Release);
            return Err(CommandError::Busy);
        }
        Ok(Self { runner_active })
    }
}

impl Drop for SessionPermit<'_> {
    fn drop(&mut self) {
        let previous = COMMAND_SESSION_COUNT.fetch_sub(1, Ordering::AcqRel);
        assert!(previous > 0);
        let active = self.runner_active.swap(false, Ordering::AcqRel);
        assert!(active);
    }
}

fn spawn_anchor() -> io::Result<Child> {
    Command::new("/bin/cat")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

fn spawn_command(spec: &CommandSpec, pgid: i32) -> Result<Child, CommandError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.environment.iter().map(|(key, value)| (key, value)))
        .stdin(if spec.stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(pgid);
    let cwd_fd = spec.cwd_handle.as_raw_fd();
    // SAFETY: the opened directory remains alive through spawn; fchdir is
    // async-signal-safe and executes before exec in the child only.
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(cwd_fd) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    command.spawn().map_err(|source| CommandError::Spawn {
        program: spec.program.clone(),
        source,
    })
}

fn checked_pid(pid: u32) -> Result<i32, CommandError> {
    i32::try_from(pid).map_err(|_| CommandError::Io {
        operation: IoOperation::Kill,
        source: io::Error::other("child PID exceeds i32"),
    })
}

struct Pipes {
    stdin: Option<ChildStdin>,
    stdin_offset: usize,
    stdout: ChildStdout,
    stderr: ChildStderr,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    output_total: usize,
    output_limit: usize,
    output_exceeded: bool,
    stdout_eof: bool,
    stderr_eof: bool,
    stdin_error: Option<io::Error>,
}

impl Pipes {
    fn new(child: &mut Child, output_limit: usize) -> Result<Self, CommandError> {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout configured as piped");
        let stderr = child.stderr.take().expect("stderr configured as piped");
        for fd in [
            stdin.as_ref().map(AsRawFd::as_raw_fd),
            Some(stdout.as_raw_fd()),
            Some(stderr.as_raw_fd()),
        ]
        .into_iter()
        .flatten()
        {
            set_nonblocking(fd)?;
        }
        Ok(Self {
            stdin,
            stdin_offset: 0,
            stdout,
            stderr,
            stdout_bytes: Vec::new(),
            stderr_bytes: Vec::new(),
            output_total: 0,
            output_limit,
            output_exceeded: false,
            stdout_eof: false,
            stderr_eof: false,
            stdin_error: None,
        })
    }

    fn pump(&mut self, input: &[u8]) -> Result<(), CommandError> {
        self.read_stdout()?;
        self.read_stderr()?;
        self.write_stdin(input);
        Ok(())
    }

    fn read_stdout(&mut self) -> Result<(), CommandError> {
        if self.stdout_eof {
            return Ok(());
        }
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        loop {
            match self.stdout.read(&mut chunk) {
                Ok(0) => {
                    self.stdout_eof = true;
                    return Ok(());
                }
                Ok(count) => {
                    retain_output(
                        &mut self.stdout_bytes,
                        &chunk[..count],
                        &mut self.output_total,
                        self.output_limit,
                        &mut self.output_exceeded,
                    );
                    if self.output_exceeded {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(source) => {
                    return Err(CommandError::Io {
                        operation: IoOperation::StdoutRead,
                        source,
                    });
                }
            }
        }
    }

    fn read_stderr(&mut self) -> Result<(), CommandError> {
        if self.stderr_eof {
            return Ok(());
        }
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        loop {
            match self.stderr.read(&mut chunk) {
                Ok(0) => {
                    self.stderr_eof = true;
                    return Ok(());
                }
                Ok(count) => {
                    retain_output(
                        &mut self.stderr_bytes,
                        &chunk[..count],
                        &mut self.output_total,
                        self.output_limit,
                        &mut self.output_exceeded,
                    );
                    if self.output_exceeded {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(source) => {
                    return Err(CommandError::Io {
                        operation: IoOperation::StderrRead,
                        source,
                    });
                }
            }
        }
    }

    fn write_stdin(&mut self, input: &[u8]) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        while self.stdin_offset < input.len() {
            match stdin.write(&input[self.stdin_offset..]) {
                Ok(0) => break,
                Ok(count) => self.stdin_offset += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                    self.stdin_error = Some(error);
                    self.stdin = None;
                    return;
                }
                Err(error) => {
                    self.stdin_error = Some(error);
                    self.stdin = None;
                    return;
                }
            }
        }
        if self.stdin_offset == input.len() {
            self.stdin = None;
        }
    }

    fn drain_until_eof(&mut self, deadline: Instant) -> Result<(), CommandError> {
        self.stdin = None;
        while !self.stdout_eof || !self.stderr_eof {
            self.read_stdout()?;
            self.read_stderr()?;
            if self.stdout_eof && self.stderr_eof {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(CommandError::Io {
                    operation: IoOperation::Drain,
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "command pipes did not close after group termination",
                    ),
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
        Ok(())
    }
}

fn set_nonblocking(fd: RawFd) -> Result<(), CommandError> {
    // SAFETY: fd is an owned live pipe descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(CommandError::Io {
            operation: IoOperation::Nonblocking,
            source: io::Error::last_os_error(),
        });
    }
    // SAFETY: F_SETFL changes only status flags for the same valid descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(CommandError::Io {
            operation: IoOperation::Nonblocking,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn retain_output(
    destination: &mut Vec<u8>,
    chunk: &[u8],
    total: &mut usize,
    limit: usize,
    exceeded: &mut bool,
) {
    let remaining = limit.saturating_sub(*total);
    let retained = remaining.min(chunk.len());
    destination.extend_from_slice(&chunk[..retained]);
    *total = total
        .checked_add(chunk.len())
        .expect("terminated command output cannot overflow usize");
    if retained != chunk.len() {
        *exceeded = true;
    }
    assert!(destination.len() <= limit);
}

enum RunCause {
    Exited(ExitStatus),
    Timeout,
    OutputLimit,
}

fn supervise(
    child: &mut Child,
    pipes: &mut Pipes,
    deadline: Instant,
    input: &[u8],
) -> Result<RunCause, CommandError> {
    loop {
        pipes.pump(input)?;
        if pipes.output_exceeded {
            return Ok(RunCause::OutputLimit);
        }
        if Instant::now() >= deadline {
            return Ok(RunCause::Timeout);
        }
        if let Some(status) = child.try_wait().map_err(|source| CommandError::Io {
            operation: IoOperation::Wait,
            source,
        })? {
            return Ok(RunCause::Exited(status));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn kill_group(pgid: i32) -> Result<(), CommandError> {
    assert!(pgid > 1);
    // SAFETY: pgid is the checked positive PID of the live owned anchor.
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
        return Err(CommandError::Io {
            operation: IoOperation::Kill,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn reap_owned<const N: usize>(
    mut children: [&mut Child; N],
    deadline: Instant,
    errors: &mut Vec<CommandError>,
) {
    let mut resolved = [false; N];
    while resolved.iter().any(|done| !done) {
        for (index, child) in children.iter_mut().enumerate() {
            if resolved[index] {
                continue;
            }
            match child.try_wait() {
                Ok(Some(_)) => resolved[index] = true,
                Ok(None) => {
                    if let Err(source) = child.kill() {
                        if source.raw_os_error() != Some(libc::ESRCH) {
                            errors.push(CommandError::Io {
                                operation: IoOperation::Kill,
                                source,
                            });
                        }
                    }
                }
                Err(source) => errors.push(CommandError::Io {
                    operation: IoOperation::Wait,
                    source,
                }),
            }
        }
        if resolved.iter().all(|done| *done) {
            return;
        }
        if Instant::now() >= deadline {
            // Continuing would release concurrency capacity while owned children
            // may still consume process-table entries. Fail the process invariant.
            std::process::abort();
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(target_os = "linux")]
fn ensure_descendant_reaper() -> Result<(), CommandError> {
    // SAFETY: PR_SET_CHILD_SUBREAPER changes only this process's child-reaping role.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(CommandError::Io {
            operation: IoOperation::Wait,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_descendant_reaper() -> Result<(), CommandError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn reap_descendants(pgid: i32, deadline: Instant, errors: &mut Vec<CommandError>) {
    loop {
        let mut status = 0;
        // SAFETY: status is writable; a negative PID selects reparented members
        // of this owned group without consuming unrelated children.
        let result = unsafe { libc::waitpid(-pgid, &mut status, libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result == -1 {
            let source = io::Error::last_os_error();
            if source.raw_os_error() == Some(libc::ECHILD) {
                return;
            }
            let _ = (errors, source);
            std::process::abort();
        }
        if Instant::now() >= deadline {
            std::process::abort();
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_descendants(_pgid: i32, _deadline: Instant, _errors: &mut Vec<CommandError>) {}

fn attach_cleanup(primary: CommandError, cleanup: Vec<CommandError>) -> CommandError {
    if cleanup.is_empty() {
        primary
    } else {
        CommandError::Cleanup {
            outcome: Box::new(CommandOutcome::Failure(Box::new(primary))),
            cleanup: cleanup.into_boxed_slice(),
        }
    }
}

fn attach_result_cleanup(
    result: Result<CommandOutput, CommandError>,
    cleanup: Vec<CommandError>,
) -> CommandError {
    let outcome = match result {
        Ok(output) => CommandOutcome::Success(output),
        Err(error) => CommandOutcome::Failure(Box::new(error)),
    };
    CommandError::Cleanup {
        outcome: Box::new(outcome),
        cleanup: cleanup.into_boxed_slice(),
    }
}

fn finish(
    cause: RunCause,
    pipes: Pipes,
    spec: &CommandSpec,
) -> Result<CommandOutput, CommandError> {
    if pipes.output_exceeded {
        return Err(CommandError::OutputLimit {
            limit_bytes: spec.output_limit_bytes,
        });
    }
    if matches!(cause, RunCause::Timeout) {
        return Err(CommandError::Timeout {
            timeout: spec.timeout,
        });
    }
    if let Some(source) = pipes.stdin_error {
        return Err(CommandError::Io {
            operation: IoOperation::StdinWrite,
            source,
        });
    }
    if pipes.stdin_offset != spec.stdin.len() {
        return Err(CommandError::Io {
            operation: IoOperation::StdinWrite,
            source: io::Error::new(
                io::ErrorKind::WriteZero,
                format!(
                    "command accepted {} of {} stdin bytes",
                    pipes.stdin_offset,
                    spec.stdin.len()
                ),
            ),
        });
    }
    let stdout = std::str::from_utf8(&pipes.stdout_bytes)
        .map_err(|source| CommandError::Utf8 {
            stream: OutputStream::Stdout,
            source,
        })?
        .to_owned();
    let stderr = std::str::from_utf8(&pipes.stderr_bytes)
        .map_err(|source| CommandError::Utf8 {
            stream: OutputStream::Stderr,
            source,
        })?
        .to_owned();
    let RunCause::Exited(status) = cause else {
        unreachable!("timeout and output limit handled above")
    };
    if !status.success() {
        return Err(CommandError::Exit {
            status_code: status.code(),
            stdout,
            stderr,
        });
    }
    Ok(CommandOutput { stdout, stderr })
}
