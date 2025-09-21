use anyhow::{Context, Result};
use std::process::{Command, Output};

/// Handles command execution with consistent error handling
#[derive(Debug, Clone, Default)]
pub struct CommandExecutor {
    pub verbose: bool,
}

impl CommandExecutor {
    /// Create a new CommandExecutor
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self { verbose: false }
    }

    /// Create a new CommandExecutor with verbose output
    pub fn new_verbose(verbose: bool) -> Self {
        Self { verbose }
    }

    /// Execute a command and return the result
    pub fn run(&self, args: &[&str]) -> Result<CommandOutput> {
        self.run_with_check(args, true)
    }

    /// Execute a command with optional error checking
    pub fn run_with_check(&self, args: &[&str], check: bool) -> Result<CommandOutput> {
        if args.is_empty() {
            anyhow::bail!("No command provided");
        }

        if self.verbose {
            eprintln!("[debug] Executing: {}", args.join(" "));
        }

        let output = Command::new(args[0])
            .args(&args[1..])
            .output()
            .with_context(|| format!("Failed to execute command: {}", args.join(" ")))?;

        let result = CommandOutput::from(output);

        if check && !result.success() {
            if self.verbose {
                eprintln!("[debug] Command failed: {}", args.join(" "));
                eprintln!("[debug] Exit code: {}", result.exit_code);
                if !result.stdout.is_empty() {
                    eprintln!("[debug] Stdout: {}", result.stdout);
                }
                if !result.stderr.is_empty() {
                    eprintln!("[debug] Stderr: {}", result.stderr);
                }
            }
            anyhow::bail!(
                "Command failed with exit code {}: {}",
                result.exit_code,
                args.join(" ")
            );
        }

        Ok(result)
    }

    /// Execute a command without checking the exit code
    pub fn run_unchecked(&self, args: &[&str]) -> Result<CommandOutput> {
        self.run_with_check(args, false)
    }
}

/// Result of command execution
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl CommandOutput {
    /// Check if the command was successful
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// Get combined output (stdout + stderr)
    pub fn combined_output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

impl From<Output> for CommandOutput {
    fn from(output: Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
        }
    }
}
