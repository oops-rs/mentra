use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParsedCommand {
    Read {
        cmd: String,
        name: String,
        path: PathBuf,
    },
    Search {
        cmd: String,
        query: Option<String>,
        path: Option<PathBuf>,
    },
    ListFiles {
        cmd: String,
        path: Option<PathBuf>,
    },
    Unknown {
        cmd: String,
    },
}

impl ParsedCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Search { .. } => "search",
            Self::ListFiles { .. } => "list_files",
            Self::Unknown { .. } => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandStage {
    pub raw: String,
    pub commands: Vec<Vec<String>>,
    pub cwd: PathBuf,
    pub parsed: ParsedCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    #[default]
    Allow,
    Prompt,
    Forbidden,
}

impl Decision {
    pub fn merge(self, other: Self) -> Self {
        use Decision::{Allow, Forbidden, Prompt};

        match (self, other) {
            (Forbidden, _) | (_, Forbidden) => Forbidden,
            (Prompt, _) | (_, Prompt) => Prompt,
            _ => Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Never,
    Always,
    #[default]
    UnlessAllowed,
    OnRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRule {
    pub program: String,
    pub args: Vec<String>,
    pub decision: Decision,
    pub justification: Option<String>,
}

impl ExecRule {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        decision: Decision,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            decision,
            justification: None,
        }
    }

    pub fn with_justification(mut self, justification: impl Into<String>) -> Self {
        self.justification = Some(justification.into());
        self
    }

    pub fn matches(&self, argv: &[String]) -> bool {
        if argv.first() != Some(&self.program) {
            return false;
        }

        let rest = &argv[1..];
        let mut index = 0usize;
        for pattern in &self.args {
            match pattern.as_str() {
                "**" => return true,
                "*" => {
                    if index >= rest.len() {
                        return false;
                    }
                    index += 1;
                }
                literal => {
                    if rest.get(index).map(|value| value.as_str()) != Some(literal) {
                        return false;
                    }
                    index += 1;
                }
            }
        }

        index == rest.len() || self.args.last().is_some_and(|value| value == "**")
    }

    pub fn summary(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    pub summary: String,
    pub decision: Decision,
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEvaluation {
    pub parsed: ParsedCommand,
    pub stages: Vec<CommandStage>,
    pub decision: Decision,
    pub matched_rules: Vec<RuleMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandParse {
    pub parsed: ParsedCommand,
    pub stages: Vec<CommandStage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub requested_timeout: Option<Duration>,
    pub justification: Option<String>,
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub status_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_rule_supports_prefix_wildcards() {
        let rule = ExecRule::new("git", ["status", "*"], Decision::Allow);
        assert!(rule.matches(&[
            "git".to_string(),
            "status".to_string(),
            "--short".to_string()
        ]));
        assert!(!rule.matches(&["git".to_string(), "diff".to_string()]));
    }
}
