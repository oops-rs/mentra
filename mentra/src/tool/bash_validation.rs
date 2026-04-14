//! Bash command validation — safety checks before shell execution.
//!
//! Provides heuristic classification and validation of shell commands to detect
//! destructive, write, or suspicious operations before they execute.

use std::path::Path;

/// Result of validating a bash command before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Command is safe to execute.
    Allow,
    /// Command should be blocked with the given reason.
    Block { reason: String },
    /// Command requires user confirmation with the given warning.
    Warn { message: String },
}

/// Semantic classification of a bash command's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIntent {
    ReadOnly,
    Write,
    Destructive,
    Network,
    ProcessManagement,
    PackageManagement,
    SystemAdmin,
    Unknown,
}

// ---------------------------------------------------------------------------
// Command lists
// ---------------------------------------------------------------------------

const WRITE_COMMANDS: &[&str] = &[
    "cp", "mv", "rm", "mkdir", "rmdir", "touch", "chmod", "chown", "chgrp", "ln", "install", "tee",
    "truncate", "shred", "mkfifo", "mknod", "dd",
];

const STATE_MODIFYING_COMMANDS: &[&str] = &[
    "apt",
    "apt-get",
    "yum",
    "dnf",
    "pacman",
    "brew",
    "pip",
    "pip3",
    "npm",
    "yarn",
    "pnpm",
    "bun",
    "cargo",
    "gem",
    "go",
    "rustup",
    "docker",
    "systemctl",
    "service",
    "mount",
    "umount",
    "kill",
    "pkill",
    "killall",
    "reboot",
    "shutdown",
    "halt",
    "poweroff",
    "useradd",
    "userdel",
    "usermod",
    "groupadd",
    "groupdel",
    "crontab",
    "at",
];

const WRITE_REDIRECTIONS: &[&str] = &[">", ">>", ">&"];

const READ_ONLY_COMMANDS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "sort",
    "uniq",
    "grep",
    "egrep",
    "fgrep",
    "find",
    "which",
    "whereis",
    "whatis",
    "man",
    "file",
    "stat",
    "du",
    "df",
    "free",
    "uptime",
    "uname",
    "hostname",
    "whoami",
    "id",
    "groups",
    "env",
    "printenv",
    "echo",
    "printf",
    "date",
    "cal",
    "bc",
    "expr",
    "test",
    "true",
    "false",
    "pwd",
    "tree",
    "diff",
    "cmp",
    "md5sum",
    "sha256sum",
    "sha1sum",
    "xxd",
    "od",
    "hexdump",
    "strings",
    "readlink",
    "realpath",
    "basename",
    "dirname",
    "seq",
    "tput",
    "column",
    "jq",
    "yq",
    "xargs",
    "tr",
    "cut",
    "paste",
    "awk",
    "sed",
    "rg",
];

const NETWORK_COMMANDS: &[&str] = &[
    "curl",
    "wget",
    "ssh",
    "scp",
    "rsync",
    "ftp",
    "sftp",
    "nc",
    "ncat",
    "telnet",
    "ping",
    "traceroute",
    "dig",
    "nslookup",
    "host",
    "whois",
    "ifconfig",
    "ip",
    "netstat",
    "ss",
    "nmap",
];

const PROCESS_COMMANDS: &[&str] = &[
    "kill", "pkill", "killall", "ps", "top", "htop", "bg", "fg", "jobs", "nohup", "disown", "wait",
    "nice", "renice",
];

const PACKAGE_COMMANDS: &[&str] = &[
    "apt", "apt-get", "yum", "dnf", "pacman", "brew", "pip", "pip3", "npm", "yarn", "pnpm", "bun",
    "cargo", "gem", "go", "rustup", "snap", "flatpak",
];

const SYSTEM_ADMIN_COMMANDS: &[&str] = &[
    "sudo",
    "su",
    "chroot",
    "mount",
    "umount",
    "fdisk",
    "parted",
    "systemctl",
    "service",
    "iptables",
    "ufw",
    "sysctl",
    "crontab",
    "at",
    "useradd",
    "userdel",
    "usermod",
    "groupadd",
    "groupdel",
    "passwd",
    "visudo",
];

const ALWAYS_DESTRUCTIVE_COMMANDS: &[&str] = &["shred", "wipefs"];

const DESTRUCTIVE_PATTERNS: &[(&str, &str)] = &[
    (
        "rm -rf /",
        "Recursive forced deletion at root — this will destroy the system",
    ),
    ("rm -rf ~", "Recursive forced deletion of home directory"),
    (
        "rm -rf *",
        "Recursive forced deletion of all files in current directory",
    ),
    ("rm -rf .", "Recursive forced deletion of current directory"),
    (
        "mkfs",
        "Filesystem creation will destroy existing data on the device",
    ),
    (
        "dd if=",
        "Direct disk write — can overwrite partitions or devices",
    ),
    ("> /dev/sd", "Writing to raw disk device"),
    (
        "chmod -R 777",
        "Recursively setting world-writable permissions",
    ),
    ("chmod -R 000", "Recursively removing all permissions"),
    (":(){ :|:& };:", "Fork bomb — will crash the system"),
];

const GIT_READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "tag",
    "stash",
    "remote",
    "fetch",
    "ls-files",
    "ls-tree",
    "cat-file",
    "rev-parse",
    "describe",
    "shortlog",
    "blame",
    "bisect",
    "reflog",
    "config",
];

const SYSTEM_PATHS: &[&str] = &[
    "/etc/", "/usr/", "/var/", "/boot/", "/sys/", "/proc/", "/dev/", "/sbin/", "/lib/", "/opt/",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check if a command is destructive and should be warned about.
#[must_use]
pub fn check_destructive(command: &str) -> ValidationResult {
    for &(pattern, warning) in DESTRUCTIVE_PATTERNS {
        if command.contains(pattern) {
            return ValidationResult::Warn {
                message: format!("Destructive command detected: {warning}"),
            };
        }
    }

    let first = extract_first_command(command);
    for &cmd in ALWAYS_DESTRUCTIVE_COMMANDS {
        if first == cmd {
            return ValidationResult::Warn {
                message: format!(
                    "Command '{cmd}' is inherently destructive and may cause data loss"
                ),
            };
        }
    }

    if command.contains("rm ") && command.contains("-r") && command.contains("-f") {
        return ValidationResult::Warn {
            message: "Recursive forced deletion detected — verify the target path is correct"
                .to_string(),
        };
    }

    ValidationResult::Allow
}

/// Check if a command targets paths outside the workspace.
#[must_use]
pub fn check_workspace_escape(command: &str) -> ValidationResult {
    let first = extract_first_command(command);
    let is_write_cmd = WRITE_COMMANDS.contains(&first.as_str())
        || STATE_MODIFYING_COMMANDS.contains(&first.as_str());

    if !is_write_cmd {
        return ValidationResult::Allow;
    }

    for sys_path in SYSTEM_PATHS {
        if command.contains(sys_path) {
            return ValidationResult::Warn {
                message:
                    "Command appears to target files outside the workspace — requires elevated permission"
                        .to_string(),
            };
        }
    }

    ValidationResult::Allow
}

/// Validate path patterns in a command.
#[must_use]
pub fn validate_paths(command: &str, workspace: &Path) -> ValidationResult {
    if command.contains("../") {
        let workspace_str = workspace.to_string_lossy();
        if !command.contains(&*workspace_str) {
            return ValidationResult::Warn {
                message: "Command contains directory traversal pattern '../' — verify the target path resolves within the workspace".to_string(),
            };
        }
    }

    if command.contains("~/") || command.contains("$HOME") {
        return ValidationResult::Warn {
            message:
                "Command references home directory — verify it stays within the workspace scope"
                    .to_string(),
        };
    }

    ValidationResult::Allow
}

/// Validate sed-specific safety.
#[must_use]
pub fn validate_sed(command: &str, read_only: bool) -> ValidationResult {
    let first = extract_first_command(command);
    if first != "sed" {
        return ValidationResult::Allow;
    }

    if read_only && command.contains(" -i") {
        return ValidationResult::Block {
            reason: "sed -i (in-place editing) is not allowed in read-only mode".to_string(),
        };
    }

    ValidationResult::Allow
}

/// Validate a command for read-only mode.
#[must_use]
pub fn validate_read_only(command: &str) -> ValidationResult {
    let first_command = extract_first_command(command);

    for &write_cmd in WRITE_COMMANDS {
        if first_command == write_cmd {
            return ValidationResult::Block {
                reason: format!(
                    "Command '{write_cmd}' modifies the filesystem and is not allowed in read-only mode"
                ),
            };
        }
    }

    for &state_cmd in STATE_MODIFYING_COMMANDS {
        if first_command == state_cmd {
            return ValidationResult::Block {
                reason: format!(
                    "Command '{state_cmd}' modifies system state and is not allowed in read-only mode"
                ),
            };
        }
    }

    if first_command == "sudo" {
        let inner = extract_sudo_inner(command);
        if !inner.is_empty() {
            let inner_result = validate_read_only(inner);
            if inner_result != ValidationResult::Allow {
                return inner_result;
            }
        }
    }

    for &redir in WRITE_REDIRECTIONS {
        if command.contains(redir) {
            return ValidationResult::Block {
                reason: format!(
                    "Command contains write redirection '{redir}' which is not allowed in read-only mode"
                ),
            };
        }
    }

    if first_command == "git" {
        return validate_git_read_only(command);
    }

    ValidationResult::Allow
}

/// Classify the semantic intent of a bash command.
#[must_use]
pub fn classify_command(command: &str) -> CommandIntent {
    let first = extract_first_command(command);

    if READ_ONLY_COMMANDS.contains(&first.as_str()) {
        if first == "sed" && command.contains(" -i") {
            return CommandIntent::Write;
        }
        return CommandIntent::ReadOnly;
    }

    if ALWAYS_DESTRUCTIVE_COMMANDS.contains(&first.as_str()) || first == "rm" {
        return CommandIntent::Destructive;
    }

    if WRITE_COMMANDS.contains(&first.as_str()) {
        return CommandIntent::Write;
    }

    if NETWORK_COMMANDS.contains(&first.as_str()) {
        return CommandIntent::Network;
    }

    if PROCESS_COMMANDS.contains(&first.as_str()) {
        return CommandIntent::ProcessManagement;
    }

    if PACKAGE_COMMANDS.contains(&first.as_str()) {
        return CommandIntent::PackageManagement;
    }

    if SYSTEM_ADMIN_COMMANDS.contains(&first.as_str()) {
        return CommandIntent::SystemAdmin;
    }

    if first == "git" {
        return classify_git_command(command);
    }

    CommandIntent::Unknown
}

/// Run the full validation pipeline on a bash command.
#[must_use]
pub fn validate_command(command: &str, workspace: &Path, read_only: bool) -> ValidationResult {
    if read_only {
        let result = validate_read_only(command);
        if result != ValidationResult::Allow {
            return result;
        }
    }

    let result = validate_sed(command, read_only);
    if result != ValidationResult::Allow {
        return result;
    }

    let result = check_destructive(command);
    if result != ValidationResult::Allow {
        return result;
    }

    let result = check_workspace_escape(command);
    if result != ValidationResult::Allow {
        return result;
    }

    validate_paths(command, workspace)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn validate_git_read_only(command: &str) -> ValidationResult {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let subcommand = parts.iter().skip(1).find(|p| !p.starts_with('-'));

    match subcommand {
        Some(&sub) if GIT_READ_ONLY_SUBCOMMANDS.contains(&sub) => ValidationResult::Allow,
        Some(&sub) => ValidationResult::Block {
            reason: format!(
                "Git subcommand '{sub}' modifies repository state and is not allowed in read-only mode"
            ),
        },
        None => ValidationResult::Allow,
    }
}

fn classify_git_command(command: &str) -> CommandIntent {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let subcommand = parts.iter().skip(1).find(|p| !p.starts_with('-'));
    match subcommand {
        Some(&sub) if GIT_READ_ONLY_SUBCOMMANDS.contains(&sub) => CommandIntent::ReadOnly,
        _ => CommandIntent::Write,
    }
}

fn extract_first_command(command: &str) -> String {
    let trimmed = command.trim();
    let mut remaining = trimmed;

    // Skip leading environment variable assignments.
    loop {
        let next = remaining.trim_start();
        if let Some(eq_pos) = next.find('=') {
            let before_eq = &next[..eq_pos];
            if !before_eq.is_empty()
                && before_eq
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let after_eq = &next[eq_pos + 1..];
                if let Some(space) = find_end_of_value(after_eq) {
                    remaining = &after_eq[space..];
                    continue;
                }
                return String::new();
            }
        }
        break;
    }

    remaining
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

fn extract_sudo_inner(command: &str) -> &str {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let sudo_idx = parts.iter().position(|&p| p == "sudo");
    match sudo_idx {
        Some(idx) => {
            let rest = &parts[idx + 1..];
            for &part in rest {
                if !part.starts_with('-') {
                    let offset = command.find(part).unwrap_or(0);
                    return &command[offset..];
                }
            }
            ""
        }
        None => "",
    }
}

fn find_end_of_value(s: &str) -> Option<usize> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }

    let first = s.as_bytes()[0];
    if first == b'"' || first == b'\'' {
        let quote = first;
        let mut i = 1;
        while i < s.len() {
            if s.as_bytes()[i] == quote && (i == 0 || s.as_bytes()[i - 1] != b'\\') {
                i += 1;
                while i < s.len() && !s.as_bytes()[i].is_ascii_whitespace() {
                    i += 1;
                }
                return if i < s.len() { Some(i) } else { None };
            }
            i += 1;
        }
        None
    } else {
        s.find(char::is_whitespace)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_read_only_commands() {
        assert_eq!(classify_command("ls -la"), CommandIntent::ReadOnly);
        assert_eq!(classify_command("cat file.txt"), CommandIntent::ReadOnly);
        assert_eq!(
            classify_command("grep -r pattern ."),
            CommandIntent::ReadOnly
        );
        assert_eq!(
            classify_command("find . -name '*.rs'"),
            CommandIntent::ReadOnly
        );
        assert_eq!(classify_command("rg pattern"), CommandIntent::ReadOnly);
    }

    #[test]
    fn classify_write_commands() {
        assert_eq!(classify_command("cp a.txt b.txt"), CommandIntent::Write);
        assert_eq!(classify_command("mv old.txt new.txt"), CommandIntent::Write);
        assert_eq!(classify_command("mkdir -p /tmp/dir"), CommandIntent::Write);
    }

    #[test]
    fn classify_destructive_commands() {
        assert_eq!(
            classify_command("rm -rf /tmp/x"),
            CommandIntent::Destructive
        );
        assert_eq!(
            classify_command("shred /dev/sda"),
            CommandIntent::Destructive
        );
    }

    #[test]
    fn classify_network_commands() {
        assert_eq!(
            classify_command("curl https://example.com"),
            CommandIntent::Network
        );
        assert_eq!(classify_command("wget file.zip"), CommandIntent::Network);
    }

    #[test]
    fn classify_sed_inplace_as_write() {
        assert_eq!(
            classify_command("sed -i 's/old/new/' file.txt"),
            CommandIntent::Write
        );
    }

    #[test]
    fn classify_sed_stdout_as_read_only() {
        assert_eq!(
            classify_command("sed 's/old/new/' file.txt"),
            CommandIntent::ReadOnly
        );
    }

    #[test]
    fn classify_git_status_as_read_only() {
        assert_eq!(classify_command("git status"), CommandIntent::ReadOnly);
        assert_eq!(
            classify_command("git log --oneline"),
            CommandIntent::ReadOnly
        );
    }

    #[test]
    fn classify_git_push_as_write() {
        assert_eq!(
            classify_command("git push origin main"),
            CommandIntent::Write
        );
    }

    #[test]
    fn blocks_rm_in_read_only() {
        assert!(matches!(
            validate_read_only("rm -rf /tmp/x"),
            ValidationResult::Block { reason } if reason.contains("rm")
        ));
    }

    #[test]
    fn allows_ls_in_read_only() {
        assert_eq!(validate_read_only("ls -la"), ValidationResult::Allow);
    }

    #[test]
    fn blocks_write_redirect_in_read_only() {
        assert!(matches!(
            validate_read_only("echo hello > file.txt"),
            ValidationResult::Block { reason } if reason.contains("redirection")
        ));
    }

    #[test]
    fn blocks_sudo_rm_in_read_only() {
        assert!(matches!(
            validate_read_only("sudo rm -rf /tmp/x"),
            ValidationResult::Block { reason } if reason.contains("rm")
        ));
    }

    #[test]
    fn blocks_git_push_in_read_only() {
        assert!(matches!(
            validate_read_only("git push origin main"),
            ValidationResult::Block { reason } if reason.contains("push")
        ));
    }

    #[test]
    fn allows_git_status_in_read_only() {
        assert_eq!(validate_read_only("git status"), ValidationResult::Allow);
    }

    #[test]
    fn warns_rm_rf_root() {
        assert!(matches!(
            check_destructive("rm -rf /"),
            ValidationResult::Warn { message } if message.contains("root")
        ));
    }

    #[test]
    fn warns_fork_bomb() {
        assert!(matches!(
            check_destructive(":(){ :|:& };:"),
            ValidationResult::Warn { message } if message.contains("Fork bomb")
        ));
    }

    #[test]
    fn allows_safe_destructive_check() {
        assert_eq!(check_destructive("ls -la"), ValidationResult::Allow);
    }

    #[test]
    fn warns_system_paths() {
        assert!(matches!(
            check_workspace_escape("cp file.txt /etc/config"),
            ValidationResult::Warn { .. }
        ));
    }

    #[test]
    fn allows_local_write() {
        assert_eq!(
            check_workspace_escape("cp file.txt ./backup/"),
            ValidationResult::Allow
        );
    }

    #[test]
    fn warns_directory_traversal() {
        let workspace = PathBuf::from("/workspace/project");
        assert!(matches!(
            validate_paths("cat ../../../etc/passwd", &workspace),
            ValidationResult::Warn { message } if message.contains("traversal")
        ));
    }

    #[test]
    fn warns_home_reference() {
        let workspace = PathBuf::from("/workspace");
        assert!(matches!(
            validate_paths("cat ~/.ssh/id_rsa", &workspace),
            ValidationResult::Warn { message } if message.contains("home directory")
        ));
    }

    #[test]
    fn full_pipeline_blocks_write_in_read_only() {
        let workspace = PathBuf::from("/workspace");
        assert!(matches!(
            validate_command("rm -rf /tmp/x", &workspace, true),
            ValidationResult::Block { .. }
        ));
    }

    #[test]
    fn full_pipeline_warns_destructive() {
        let workspace = PathBuf::from("/workspace");
        assert!(matches!(
            validate_command("rm -rf /", &workspace, false),
            ValidationResult::Warn { .. }
        ));
    }

    #[test]
    fn full_pipeline_allows_safe_read() {
        let workspace = PathBuf::from("/workspace");
        assert_eq!(
            validate_command("ls -la", &workspace, true),
            ValidationResult::Allow
        );
    }

    #[test]
    fn extracts_command_from_env_prefix() {
        assert_eq!(extract_first_command("FOO=bar ls -la"), "ls");
        assert_eq!(extract_first_command("A=1 B=2 echo hello"), "echo");
    }

    #[test]
    fn extracts_plain_command() {
        assert_eq!(extract_first_command("grep -r pattern ."), "grep");
    }
}
