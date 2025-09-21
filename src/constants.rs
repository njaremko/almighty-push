/// Constants used throughout the application

/// State file for persisting data between runs
pub const STATE_FILE: &str = ".almighty";

/// Default base branch
pub const DEFAULT_BASE_BRANCH: &str = "main";

/// Default remote name
pub const DEFAULT_REMOTE: &str = "origin";

/// Maximum operations to check in jj op log
pub const MAX_OPS_TO_CHECK: usize = 50;

/// Branch prefix for push branches
pub const PUSH_BRANCH_PREFIX: &str = "push-";

/// Branch prefix for changes branches
pub const CHANGES_BRANCH_PREFIX: &str = "changes/";
