use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct CliFixture {
    root: PathBuf,
    workspace: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    remote_state: PathBuf,
    pr_state: PathBuf,
}

#[derive(Default)]
pub struct CliFixtureOptions {
    pub initial_state: Option<Box<[u8]>>,
    pub fail_github_observations: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRecord {
    pub program: String,
    pub operation: String,
    pub lock_path_present: bool,
}

impl CliFixture {
    pub fn new(label: &str, options: CliFixtureOptions) -> Self {
        let root = unique_directory(label);
        let workspace = root.join("workspace");
        let bin = root.join("bin");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join(".jj")).unwrap();
        fs::create_dir(&bin).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let log = root.join("commands.log");
        let remote_state = root.join("remote-present");
        let pr_state = root.join("pr-present");
        let fail_gh_reads = root.join("fail-gh-reads");
        let script = fake_script(&workspace, &log, &remote_state, &pr_state, &fail_gh_reads);
        for program in ["jj", "gh"] {
            let path = bin.join(program);
            fs::write(&path, &script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        if options.fail_github_observations {
            fs::write(&fail_gh_reads, "fail").unwrap();
        }
        if let Some(bytes) = options.initial_state {
            let state_directory = workspace.join(".jj/almighty-push");
            fs::create_dir(&state_directory).unwrap();
            fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o700)).unwrap();
            let state_path = state_directory.join("state-v3.json");
            fs::write(&state_path, bytes).unwrap();
            fs::set_permissions(state_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self {
            root,
            workspace,
            bin,
            log,
            remote_state,
            pr_state,
        }
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let mut path = OsString::from(self.bin.as_os_str());
        path.push(OsStr::new(":"));
        path.push(inherited_path);
        Command::new(env!("CARGO_BIN_EXE_almighty-push"))
            .args(args)
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.root)
            .output()
            .unwrap()
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn state_directory(&self) -> PathBuf {
        self.workspace.join(".jj/almighty-push")
    }

    pub fn remote_was_mutated(&self) -> bool {
        self.remote_state.exists()
    }

    pub fn pr_was_mutated(&self) -> bool {
        self.pr_state.exists()
    }

    pub fn records(&self) -> Vec<CommandRecord> {
        match fs::read_to_string(&self.log) {
            Ok(log) => log
                .lines()
                .map(|line| {
                    let fields = line.split('\t').collect::<Vec<_>>();
                    assert_eq!(fields.len(), 3, "malformed fake command record");
                    CommandRecord {
                        program: fields[0].to_owned(),
                        operation: fields[1].to_owned(),
                        lock_path_present: fields[2] == "locked",
                    }
                })
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("command log read failed: {error}"),
        }
    }

    pub fn json_stdout(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON: {error}; stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1_000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-cli-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path.canonicalize().unwrap(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}

fn shell_literal(path: &Path) -> String {
    let value = path.to_str().expect("temporary path is UTF-8");
    assert!(!value.contains('\''));
    format!("'{value}'")
}

fn fake_script(
    workspace: &Path,
    log: &Path,
    remote: &Path,
    pr: &Path,
    fail_gh_reads: &Path,
) -> String {
    let workspace = shell_literal(workspace);
    let log = shell_literal(log);
    let remote = shell_literal(remote);
    let pr = shell_literal(pr);
    let fail_gh_reads = shell_literal(fail_gh_reads);
    format!(
        r#"#!/bin/sh
set -eu
workspace={workspace}
log={log}
remote_state={remote}
pr_state={pr}
fail_gh_reads={fail_gh_reads}
lock_state=unlocked
if test -f "$workspace/.jj/almighty-push/lock"; then
  lock_state=locked
fi
program=${{0##*/}}
operation=read
case "$program:$*" in
  "jj:--ignore-working-copy git fetch"*) operation=fetch ;;
  "jj:--ignore-working-copy git push"*) operation=push ;;
  "jj:--ignore-working-copy rebase"*) operation=rebase ;;
  "gh:"*)
    case " $* " in
      *" --method POST "*|*" --method PATCH "*|*" --method DELETE "*) operation=gh-mutate ;;
      *) operation=gh-read ;;
    esac
    ;;
esac
printf '%s\t%s\t%s\n' "$program" "$operation" "$lock_state" >> "$log"

if test "$program" = jj; then
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = workspace; then
    printf '%s\n' "$workspace"
    exit 0
  fi
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = git && test "${{3-}}" = remote; then
    printf 'origin https://github.com/source/project.git\n'
    exit 0
  fi
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = git && test "${{3-}}" = fetch; then
    exit 0
  fi
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = bookmark; then
    if test -f "$remote_state"; then
      printf '%s\n' '{{"name":"almighty-push/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","remote":"origin","present":true,"conflict":false,"normal_target":{{"change_id":"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","commit_id":"1111111111111111111111111111111111111111","description":"root | description"}}}}'
    fi
    exit 0
  fi
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = log; then
    case " $* " in
      *"conflicts()"*) exit 0 ;;
      *) printf '%s\n' '{{"change_id":"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","commit_id":"1111111111111111111111111111111111111111","description":"root | description"}}' ;;
    esac
    exit 0
  fi
  if test "${{1-}}" = --ignore-working-copy && test "${{2-}}" = git && test "${{3-}}" = push; then
    : > "$remote_state"
    exit 0
  fi
  printf 'unsupported fake jj invocation\n' >&2
  exit 91
fi

method=GET
endpoint=
previous=
for argument in "$@"; do
  if test "$previous" = --method; then method=$argument; fi
  case "$argument" in
    repos/*) endpoint=$argument ;;
  esac
  previous=$argument
done
if test "$method" = GET && test -f "$fail_gh_reads"; then
  printf 'injected GitHub observation failure\n' >&2
  exit 93
fi
if test "$method" != GET; then
  cat >/dev/null
fi
case "$method:$endpoint" in
  "GET:repos/source/project")
    printf '%s\n' '{{"full_name":"source/project","default_branch":"main"}}'
    ;;
  "GET:repos/source/project/branches/main")
    printf '%s\n' '{{"name":"main"}}'
    ;;
  "GET:repos/source/project/git/matching-refs/heads/almighty-push%2F"*)
    if test -f "$remote_state"; then
      printf '%s\n' '[{{"ref":"refs/heads/almighty-push/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","object":{{"type":"commit","sha":"1111111111111111111111111111111111111111"}}}}]'
    else
      printf '%s\n' '[]'
    fi
    ;;
  "GET:repos/source/project/pulls?"*)
    if test -f "$pr_state"; then
      printf '%s\n' '[{{"number":7,"state":"open","merged_at":null,"title":"root | description","head_ref":"almighty-push/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","head_sha":"1111111111111111111111111111111111111111","head_repository":"source/project","base_ref":"main","base_repository":"source/project"}}]'
    else
      printf '%s\n' '[]'
    fi
    ;;
  "GET:repos/source/project/pulls/7")
    printf '%s\n' '{{"number":7,"state":"open","merged_at":null,"title":"root | description","head_ref":"almighty-push/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk","head_sha":"1111111111111111111111111111111111111111","head_repository":"source/project","base_ref":"main","base_repository":"source/project","body":"<!-- almighty-push:stack:v1:start -->\nSource repository: `github.com/source/project`\nChange ID: `kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk`\nActive stack (base to tip):\n- `kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk` (current)\n<!-- almighty-push:stack:v1:end -->"}}'
    ;;
  "POST:repos/source/project/pulls")
    : > "$pr_state"
    printf '%s\n' '{{"number":7}}'
    ;;
  *)
    printf 'unsupported fake gh invocation: %s %s\n' "$method" "$endpoint" >&2
    exit 92
    ;;
esac
"#
    )
}
