use almighty_push::command::{CommandError, CommandExecutor, CommandOutput, CommandSpec};
use almighty_push::config::{ConfigInput, ConfigResolver, TipSelection};
use almighty_push::domain::{HeadRef, Limits, RemoteName};
use almighty_push::state::{StateStore, StateV3};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct ResolverExecutor(Mutex<VecDeque<CommandOutput>>);

impl CommandExecutor for ResolverExecutor {
    fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        Ok(self.0.lock().unwrap().pop_front().unwrap())
    }
}

fn resolved(root: &Path) -> almighty_push::config::ResolvedConfig {
    let executor = ResolverExecutor(Mutex::new(VecDeque::from([
        CommandOutput {
            stdout: root.display().to_string(),
            stderr: String::new(),
        },
        CommandOutput {
            stdout: "origin https://github.com/source/project.git\n".to_owned(),
            stderr: String::new(),
        },
    ])));
    ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    )
    .resolve(
        &ConfigInput {
            remote: Some(RemoteName::parse("origin").unwrap()),
            repository: None,
            base: Some(HeadRef::parse("main").unwrap()),
            tip_selection: TipSelection::ExplicitRevset("@".to_owned()),
            limits: Limits::default(),
            github_enabled: false,
        },
        root,
    )
    .unwrap()
}

#[test]
fn read_only_missing_state_is_empty_without_creating_the_namespace() {
    let root = unique_directory("missing");
    fs::create_dir(root.join(".jj")).unwrap();
    let config = resolved(&root);

    let state = StateStore::load_read_only_from_config(&config).unwrap();

    assert_eq!(state, StateV3::empty(config.scope().clone()));
    assert!(!root.join(".jj/almighty-push").exists());
    fs::remove_dir_all(root).unwrap();
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1_000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-read-only-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path.canonicalize().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}
