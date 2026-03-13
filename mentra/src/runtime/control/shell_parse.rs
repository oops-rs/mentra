use std::path::{Path, PathBuf};

use super::{CommandParse, CommandStage, ParsedCommand};

pub(crate) fn parse_command(command: &str, cwd: &Path) -> CommandParse {
    let unwrapped = unwrap_shell(command);
    let Some(tokens) = tokenize(&unwrapped) else {
        return CommandParse {
            parsed: ParsedCommand::Unknown { cmd: unwrapped },
            stages: Vec::new(),
        };
    };

    let pipelines = split_pipelines(&tokens);
    let mut stages = Vec::new();
    let mut effective_cwd = cwd.to_path_buf();
    for pipeline in pipelines {
        if pipeline.is_empty() {
            continue;
        }

        if pipeline.len() == 1
            && let Some(next_dir) = parse_cd(&pipeline[0], &effective_cwd)
        {
            effective_cwd = next_dir;
            continue;
        }

        let raw = pipeline
            .iter()
            .map(|argv| argv.join(" "))
            .collect::<Vec<_>>()
            .join(" | ");
        let parsed = classify_pipeline(&pipeline, &effective_cwd, &raw);
        stages.push(CommandStage {
            raw,
            commands: pipeline,
            cwd: effective_cwd.clone(),
            parsed,
        });
    }

    let parsed = merge_stage_classification(&stages, &unwrapped);
    CommandParse { parsed, stages }
}

fn merge_stage_classification(stages: &[CommandStage], fallback_cmd: &str) -> ParsedCommand {
    if stages.is_empty() {
        return ParsedCommand::Unknown {
            cmd: fallback_cmd.to_string(),
        };
    }

    if stages
        .iter()
        .any(|stage| matches!(stage.parsed, ParsedCommand::Unknown { .. }))
    {
        return ParsedCommand::Unknown {
            cmd: fallback_cmd.to_string(),
        };
    }

    stages[0].parsed.clone()
}

fn unwrap_shell(command: &str) -> String {
    let mut current = command.trim().to_string();
    for _ in 0..4 {
        let Some(tokens) = tokenize(&current) else {
            break;
        };
        if tokens.len() < 3 {
            break;
        }
        let Some(shell) = tokens.first().map(|value| value.as_str()) else {
            break;
        };
        let Some(flag) = tokens.get(1).map(|value| value.as_str()) else {
            break;
        };
        let shell_match = matches!(shell, "bash" | "sh" | "zsh");
        let flag_match = matches!(flag, "-c" | "-lc");
        if !shell_match || !flag_match {
            break;
        }
        current = tokens[2].clone();
    }
    current
}

fn split_pipelines(tokens: &[String]) -> Vec<Vec<Vec<String>>> {
    let mut sequences = Vec::new();
    let mut pipeline = Vec::new();
    let mut current = Vec::new();

    for token in tokens {
        match token.as_str() {
            "|" => {
                if !current.is_empty() {
                    pipeline.push(std::mem::take(&mut current));
                }
            }
            "&&" | "||" | ";" => {
                if !current.is_empty() {
                    pipeline.push(std::mem::take(&mut current));
                }
                if !pipeline.is_empty() {
                    sequences.push(std::mem::take(&mut pipeline));
                }
            }
            _ => current.push(token.clone()),
        }
    }

    if !current.is_empty() {
        pipeline.push(current);
    }
    if !pipeline.is_empty() {
        sequences.push(pipeline);
    }

    sequences
}

fn classify_pipeline(pipeline: &[Vec<String>], cwd: &Path, raw: &str) -> ParsedCommand {
    if pipeline.is_empty() {
        return ParsedCommand::Unknown {
            cmd: raw.to_string(),
        };
    }

    let primary = classify_argv(&pipeline[0], cwd);
    if matches!(primary, ParsedCommand::Unknown { .. }) {
        return ParsedCommand::Unknown {
            cmd: raw.to_string(),
        };
    }

    for helper in pipeline.iter().skip(1) {
        if !is_transparent_pipeline_helper(helper) {
            return ParsedCommand::Unknown {
                cmd: raw.to_string(),
            };
        }
    }

    primary
}

fn classify_argv(argv: &[String], cwd: &Path) -> ParsedCommand {
    if argv.is_empty() {
        return ParsedCommand::Unknown { cmd: String::new() };
    }

    let name = argv[0].as_str();
    match name {
        "cat" | "bat" | "less" | "more" => {
            let path = find_last_positional(argv);
            path.map_or_else(
                || ParsedCommand::Unknown {
                    cmd: argv.join(" "),
                },
                |value| ParsedCommand::Read {
                    cmd: argv.join(" "),
                    name: name.to_string(),
                    path: resolve_command_path(cwd, value),
                },
            )
        }
        "head" | "tail" => {
            let path = find_last_positional(argv);
            path.map_or_else(
                || ParsedCommand::Unknown {
                    cmd: argv.join(" "),
                },
                |value| ParsedCommand::Read {
                    cmd: argv.join(" "),
                    name: name.to_string(),
                    path: resolve_command_path(cwd, value),
                },
            )
        }
        "sed" if argv.get(1).map(|value| value.as_str()) == Some("-n") => {
            let path = find_last_positional(argv);
            path.map_or_else(
                || ParsedCommand::Unknown {
                    cmd: argv.join(" "),
                },
                |value| ParsedCommand::Read {
                    cmd: argv.join(" "),
                    name: name.to_string(),
                    path: resolve_command_path(cwd, value),
                },
            )
        }
        "rg" if argv.iter().any(|token| token == "--files") => ParsedCommand::ListFiles {
            cmd: argv.join(" "),
            path: find_last_positional(argv).map(|value| resolve_command_path(cwd, value)),
        },
        "rg" | "grep" | "ag" => classify_search(argv, cwd),
        "ls" | "tree" | "eza" | "fd" => ParsedCommand::ListFiles {
            cmd: argv.join(" "),
            path: find_last_positional(argv).map(|value| resolve_command_path(cwd, value)),
        },
        "find" if looks_like_file_listing(argv) => ParsedCommand::ListFiles {
            cmd: argv.join(" "),
            path: find_find_path(argv).map(|value| resolve_command_path(cwd, value)),
        },
        _ => ParsedCommand::Unknown {
            cmd: argv.join(" "),
        },
    }
}

fn classify_search(argv: &[String], cwd: &Path) -> ParsedCommand {
    let mut query = None;
    let mut path = None;

    for token in argv.iter().skip(1) {
        if token.starts_with('-') {
            continue;
        }
        if query.is_none() {
            query = Some(token.clone());
        } else {
            path = Some(resolve_command_path(cwd, token));
        }
    }

    ParsedCommand::Search {
        cmd: argv.join(" "),
        query,
        path,
    }
}

fn find_last_positional(argv: &[String]) -> Option<&str> {
    argv.iter()
        .skip(1)
        .rev()
        .find(|token| !token.starts_with('-'))
        .map(|value| value.as_str())
}

fn looks_like_file_listing(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|window| window[0] == "-type" && window[1] == "f")
}

fn find_find_path(argv: &[String]) -> Option<&str> {
    argv.iter()
        .skip(1)
        .find(|token| !token.starts_with('-') && !token.contains('='))
        .map(|value| value.as_str())
}

fn is_transparent_pipeline_helper(argv: &[String]) -> bool {
    matches!(
        argv.first().map(|value| value.as_str()),
        Some("head") | Some("tail") | Some("nl")
    ) || argv.first().map(|value| value.as_str()) == Some("sed")
        && argv.get(1).map(|value| value.as_str()) == Some("-n")
        || argv.first().map(|value| value.as_str()) == Some("wc")
            && argv.get(1).map(|value| value.as_str()) == Some("-l")
}

fn parse_cd(argv: &[String], cwd: &Path) -> Option<PathBuf> {
    if argv.first().map(|value| value.as_str()) != Some("cd") || argv.len() != 2 {
        return None;
    }

    Some(resolve_command_path(cwd, &argv[1]))
}

fn resolve_command_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

fn tokenize(input: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut single_quoted = false;
    let mut double_quoted = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !double_quoted => {
                single_quoted = !single_quoted;
            }
            '"' if !single_quoted => {
                double_quoted = !double_quoted;
            }
            '\\' if !single_quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' | '\n' if !single_quoted && !double_quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '&' | '|' | ';' if !single_quoted && !double_quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                match ch {
                    '&' if chars.peek() == Some(&'&') => {
                        chars.next();
                        tokens.push("&&".to_string());
                    }
                    '|' if chars.peek() == Some(&'|') => {
                        chars.next();
                        tokens.push("||".to_string());
                    }
                    '&' => current.push('&'),
                    '|' => tokens.push("|".to_string()),
                    ';' => tokens.push(";".to_string()),
                    _ => {}
                }
            }
            _ => current.push(ch),
        }
    }

    if single_quoted || double_quoted {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_safe_reads_and_searches() {
        let cwd = PathBuf::from("/tmp/repo");

        let read = parse_command("cat README.md", &cwd);
        assert!(matches!(read.parsed, ParsedCommand::Read { .. }));

        let search = parse_command("rg -n TODO src", &cwd);
        assert!(matches!(search.parsed, ParsedCommand::Search { .. }));

        let list = parse_command("find . -type f", &cwd);
        assert!(matches!(list.parsed, ParsedCommand::ListFiles { .. }));
    }

    #[test]
    fn unwraps_shell_wrappers_and_transparent_helpers() {
        let cwd = PathBuf::from("/tmp/repo");

        let parsed = parse_command("bash -lc 'rg --files | head -n 20'", &cwd);
        assert!(matches!(parsed.parsed, ParsedCommand::ListFiles { .. }));
        assert_eq!(parsed.stages.len(), 1);
    }

    #[test]
    fn tracks_cd_for_following_stage() {
        let cwd = PathBuf::from("/tmp/repo");
        let parsed = parse_command("cd docs && cat guide.md", &cwd);
        assert_eq!(parsed.stages.len(), 1);
        assert_eq!(parsed.stages[0].cwd, cwd.join("docs"));
        assert!(matches!(parsed.parsed, ParsedCommand::Read { .. }));
    }

    #[test]
    fn flags_mutating_pipeline_stage_as_unknown() {
        let cwd = PathBuf::from("/tmp/repo");
        let parsed = parse_command("rg -l TODO src | xargs perl -pi -e 's/a/b/'", &cwd);
        assert!(matches!(parsed.parsed, ParsedCommand::Unknown { .. }));
    }
}
