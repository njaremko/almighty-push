use anyhow::{Context, Result};
use std::process::{Command, Output};

/// Handles command execution with consistent error handling
#[derive(Debug, Clone, Default)]
pub struct CommandExecutor {
    pub verbose: bool,
    pub dry_run: bool,
}

impl CommandExecutor {
    /// Create a new CommandExecutor
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            verbose: false,
            dry_run: false,
        }
    }

    /// Create a new CommandExecutor with verbose output
    pub fn new_verbose(verbose: bool) -> Self {
        Self {
            verbose,
            dry_run: false,
        }
    }

    /// Set dry-run mode
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
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

        // Check if this is a mutating command
        let is_mutating = self.is_mutating_command(args);

        if self.dry_run && is_mutating {
            eprintln!("[dry-run] Would execute: {}", args.join(" "));
            // Return mock success for dry-run
            return Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        if self.verbose {
            eprintln!("[debug] Executing: {}", args.join(" "));
        }

        let output = Command::new(args[0])
            .args(&args[1..])
            .output()
            .with_context(|| format!("Failed to execute command: {}", args.join(" ")))?;

        let result = CommandOutput::from(output);

        // In verbose mode, always show output for all commands
        if self.verbose {
            eprintln!("[debug] Exit code: {}", result.exit_code);
            if !result.stdout.trim().is_empty() {
                eprintln!("[debug] Output:\n{}", result.stdout.trim());
            }
            if !result.stderr.trim().is_empty() {
                eprintln!("[debug] Stderr:\n{}", result.stderr.trim());
            }
        }

        if check && !result.success() {
            if !self.verbose {
                // Only show error details if not already shown in verbose mode
                eprintln!("[debug] Command failed: {}", args.join(" "));
                eprintln!("[debug] Exit code: {}", result.exit_code);
                if !result.stderr.is_empty() {
                    eprintln!("[debug] Error: {}", result.stderr.trim());
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

    /// Check if a command is mutating (modifies state)
    fn is_mutating_command(&self, args: &[&str]) -> bool {
        if args.is_empty() {
            return false;
        }

        match args[0] {
            "jj" => {
                if args.len() < 2 {
                    return false;
                }
                match args[1] {
                    "git" => {
                        if args.len() < 3 {
                            return false;
                        }
                        matches!(args[2], "push" | "fetch")
                    }
                    "bookmark" => {
                        if args.len() < 3 {
                            return false;
                        }
                        matches!(args[2], "create" | "delete" | "move" | "set")
                    }
                    // Read-only jj commands
                    "log" | "show" | "status" | "st" | "diff" | "op" => false,
                    // Mutating jj commands
                    _ => true,
                }
            }
            "gh" => {
                if args.len() < 2 {
                    return false;
                }
                match args[1] {
                    "pr" => {
                        if args.len() < 3 {
                            return false;
                        }
                        matches!(args[2], "create" | "close" | "reopen" | "edit")
                    }
                    _ => false,
                }
            }
            _ => false,
        }
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
