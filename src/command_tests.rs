use crate::command::{CommandError, CommandRunner, CommandSpec, OutputStream, SpecError};
use crate::domain::{LimitValues, Limits};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const OUTPUT_LIMIT_BYTES: usize = 65_536;
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct ScriptFixture {
    root: PathBuf,
    script: PathBuf,
}

impl ScriptFixture {
    fn new(body: &str) -> Self {
        let root = unique_temp_directory("script");
        let script = root.join("command.sh");
        fs::write(&script, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();
        Self { root, script }
    }

    fn spec(&self, stdin: &[u8]) -> CommandSpec {
        command_spec_with_timeout(
            self.script.clone().into_os_string(),
            Box::new([]),
            self.root.clone(),
            stdin.to_vec().into_boxed_slice(),
            1_000,
        )
    }

    fn spec_with_timeout(&self, stdin: &[u8], timeout_ms: u64) -> CommandSpec {
        command_spec_with_timeout(
            self.script.clone().into_os_string(),
            Box::new([]),
            self.root.clone(),
            stdin.to_vec().into_boxed_slice(),
            timeout_ms,
        )
    }
}

impl Drop for ScriptFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn runner() -> CommandRunner {
    // SAFETY: this test binary coordinates all child creation through CommandRunner.
    unsafe { CommandRunner::assume_process_child_authority() }
}

fn test_limits(timeout_ms: u64) -> Limits {
    Limits::new(LimitValues {
        github_page_size: 8,
        command_output_bytes_max: OUTPUT_LIMIT_BYTES as u64,
        command_timeout_ms: timeout_ms,
        ..LimitValues::default()
    })
    .unwrap()
}

fn command_spec(
    program: OsString,
    args: Box<[OsString]>,
    cwd: PathBuf,
    stdin: Box<[u8]>,
) -> CommandSpec {
    command_spec_with_timeout(program, args, cwd, stdin, 1_000)
}

fn command_spec_with_timeout(
    program: OsString,
    args: Box<[OsString]>,
    cwd: PathBuf,
    stdin: Box<[u8]>,
    timeout_ms: u64,
) -> CommandSpec {
    CommandSpec::new(
        program,
        args,
        cwd,
        stdin,
        Box::new([]),
        test_limits(timeout_ms),
    )
    .unwrap()
}

#[test]
fn successful_command_returns_bounded_stdout_and_stderr() {
    let fixture =
        ScriptFixture::new("read value\nprintf 'out:%s' \"$value\"\nprintf 'diagnostic' >&2");
    let output = runner().run(&fixture.spec(b"input\n")).unwrap();

    assert_eq!(output.stdout, "out:input");
    assert_eq!(output.stderr, "diagnostic");
}

#[test]
fn successful_exit_requires_complete_stdin_delivery() {
    let fixture = ScriptFixture::new("exit 0");
    let input = vec![b'x'; 4 * 1024 * 1024];
    let error = runner().run(&fixture.spec(&input)).unwrap_err();

    assert!(matches!(
        error,
        CommandError::Io {
            operation: crate::command::IoOperation::StdinWrite,
            ..
        }
    ));
}

#[test]
fn nonzero_exit_is_never_an_empty_success() {
    let fixture = ScriptFixture::new("printf 'missing' >&2\nexit 23");
    let error = runner().run(&fixture.spec(&[])).unwrap_err();

    assert!(matches!(
        error,
        CommandError::Exit {
            status_code: Some(23),
            stdout,
            stderr,
        } if stdout.is_empty() && stderr == "missing"
    ));
}

#[test]
fn timeout_kills_the_process_group_and_returns_promptly() {
    let fixture = ScriptFixture::new("/bin/sleep 10");
    let started = Instant::now();
    let error = runner()
        .run(&fixture.spec_with_timeout(&[], 100))
        .unwrap_err();

    assert!(matches!(
        error,
        CommandError::Timeout { timeout } if timeout == Duration::from_millis(100)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn one_runner_owns_at_most_one_child_session() {
    let slow = ScriptFixture::new("printf started > started; /bin/sleep 10");
    let quick = ScriptFixture::new("exit 0");
    let runner = Arc::new(runner());
    let slow_spec = slow.spec(&[]);
    let owner = {
        let runner = Arc::clone(&runner);
        std::thread::spawn(move || runner.run(&slow_spec))
    };
    for _ in 0..100 {
        if slow.root.join("started").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(slow.root.join("started").exists());
    assert!(matches!(
        runner.run(&quick.spec(&[])),
        Err(CommandError::Busy)
    ));
    assert!(matches!(
        owner.join().unwrap(),
        Err(CommandError::Timeout { .. })
    ));
}

#[test]
fn leader_exit_does_not_leave_a_redirected_descendant_alive() {
    let fixture = ScriptFixture::new(
        "/bin/sleep 10 </dev/null >/dev/null 2>&1 & echo $! > descendant.pid; exit 0",
    );
    runner().run(&fixture.spec(&[])).unwrap();

    let pid: i32 = fs::read_to_string(fixture.root.join("descendant.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let disappeared = (0..100).any(|_| {
        // SAFETY: signal zero performs no mutation and the fixture produced a positive PID.
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
        false
    });
    assert!(disappeared);
}

#[test]
fn leader_exit_cannot_extend_pipe_joins_beyond_the_deadline() {
    let fixture = ScriptFixture::new("/bin/sleep 2 & exit 0");
    let started = Instant::now();
    runner().run(&fixture.spec(&[])).unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn output_limit_kills_an_unbounded_writer() {
    let fixture = ScriptFixture::new("while :; do printf '0123456789abcdef'; done");
    let started = Instant::now();
    let error = runner().run(&fixture.spec(&[])).unwrap_err();

    assert!(matches!(
        error,
        CommandError::OutputLimit {
            limit_bytes: OUTPUT_LIMIT_BYTES,
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn exact_combined_output_bound_is_accepted() {
    let fixture = ScriptFixture::new("printf '%032768d' 0; printf '%032768d' 0 >&2");
    let output = runner().run(&fixture.spec(&[])).unwrap();

    assert_eq!(output.stdout.len(), 32_768);
    assert_eq!(output.stderr.len(), 32_768);
}

#[test]
fn a_fast_successful_writer_cannot_race_past_the_output_limit() {
    let fixture = ScriptFixture::new("printf '%065537d' 0");
    let error = runner().run(&fixture.spec(&[])).unwrap_err();

    assert!(matches!(
        error,
        CommandError::OutputLimit {
            limit_bytes: OUTPUT_LIMIT_BYTES,
        }
    ));
}

#[test]
fn routine_formatting_never_contains_command_secrets() {
    const SECRET: &str = "token-super-secret-sentinel";
    let fixture = ScriptFixture::new(&format!("printf '{SECRET}'; printf '{SECRET}' >&2; exit 9"));
    let spec = command_spec(
        fixture.script.clone().into_os_string(),
        Box::new([OsString::from(SECRET)]),
        fixture.root.clone(),
        SECRET.as_bytes().to_vec().into_boxed_slice(),
    );
    assert!(!format!("{spec:?}").contains(SECRET));

    let error = runner().run(&spec).unwrap_err();
    assert!(!error.to_string().contains(SECRET));
    assert!(!format!("{error:?}").contains(SECRET));
}

#[test]
fn invalid_utf8_is_a_typed_boundary_error() {
    let fixture = ScriptFixture::new("printf '\\377'");
    let error = runner().run(&fixture.spec(&[])).unwrap_err();

    assert!(matches!(
        error,
        CommandError::Utf8 {
            stream: OutputStream::Stdout,
            ..
        }
    ));
}

#[test]
fn spawn_failure_preserves_the_program_identity() {
    let root = unique_temp_directory("missing");
    let program = OsString::from("/definitely/missing/almighty-push-command");
    let spec = command_spec(program.clone(), Box::new([]), root.clone(), Box::new([]));
    let error = runner().run(&spec).unwrap_err();

    assert!(matches!(
        error,
        CommandError::Spawn {
            program: failed_program,
            ..
        } if failed_program == program
    ));
    fs::remove_dir(root).unwrap();
}

#[test]
fn opened_working_directory_cannot_be_redirected_by_path_replacement() {
    let root = unique_temp_directory("cwd");
    let original = root.join("original");
    let moved = root.join("moved");
    fs::create_dir(&original).unwrap();
    let original = original.canonicalize().unwrap();
    let spec = command_spec(
        OsString::from("/bin/pwd"),
        Box::new([]),
        original.clone(),
        Box::new([]),
    );
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();

    let output = runner().run(&spec).unwrap();
    assert_eq!(
        output.stdout.trim(),
        moved.canonicalize().unwrap().to_str().unwrap()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn command_specs_reject_invalid_resource_contracts() {
    let fixture = ScriptFixture::new("exit 0");
    let make = |program, args, stdin| {
        CommandSpec::new(
            program,
            args,
            fixture.root.clone(),
            stdin,
            Box::new([]),
            test_limits(1_000),
        )
    };

    assert!(matches!(
        make(OsString::from("relative"), Box::new([]), Box::new([])),
        Err(SpecError::ProgramNotAbsolute)
    ));
    let spec = make(
        fixture.script.clone().into_os_string(),
        Box::new([]),
        Box::new([]),
    )
    .unwrap();
    assert_eq!(spec.timeout(), Duration::from_millis(1_000));
    assert_eq!(spec.output_limit_bytes(), OUTPUT_LIMIT_BYTES);
    assert!(matches!(
        make(
            fixture.script.clone().into_os_string(),
            vec![OsString::new(); 513].into_boxed_slice(),
            Box::new([]),
        ),
        Err(SpecError::ArgumentCount { count: 513 })
    ));
    assert!(matches!(
        make(
            fixture.script.clone().into_os_string(),
            Box::new([]),
            vec![0; 4 * 1024 * 1024 + 1].into_boxed_slice(),
        ),
        Err(SpecError::StdinLimit { .. })
    ));
    assert!(matches!(
        make(
            fixture.script.clone().into_os_string(),
            Box::new([OsString::from_vec(b"bad\0argument".to_vec())]),
            Box::new([]),
        ),
        Err(SpecError::InteriorNul)
    ));
}

fn unique_temp_directory(label: &str) -> PathBuf {
    for _ in 0..1000 {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "almighty-push-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => return root.canonicalize().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("failed to create fixture: {error}"),
        }
    }
    panic!("exhausted temporary fixture attempts")
}
