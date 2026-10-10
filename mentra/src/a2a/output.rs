//! A remote agent's own words, framed so a model can tell them from anyone
//! else's.
//!
//! Everything a remote agent wrote is placed between two delimiter lines
//! naming the agent and clipped to the client's output bound; everything a
//! harness says around it — headlines, ids, what to call next — stays outside
//! them. The text is external content with the standing of a web page, and
//! the delimiters are what keep it from reading as the harness's own.
//!
//! [`A2aAgentTool`](crate::a2a::A2aAgentTool) frames every reply this way, and
//! a host offering a remote agent through its own tool calls the same
//! functions so the framing is one convention rather than two.

use a2a_lf::{AgentCard, Message, Part, PartContent, Task, TaskState};

/// Bound on the card text — description and skills — in a summary.
const MAX_CARD_TEXT_BYTES: usize = 2 * 1024;
/// Bound on one skill's description within that.
const MAX_SKILL_TEXT_BYTES: usize = 240;
/// Bound on how many skills are listed.
const MAX_SKILLS: usize = 16;

/// A task's text between the delimiters, or an empty string when the task
/// carries no text at all.
///
/// A completed task's answer is its artifacts, so its status message is only
/// shown when there are none; any other state's status message — the question
/// an input-required task is asking, the reason a failed one failed — is shown
/// first.
pub fn remote_output(agent: &str, task: &Task, max_output_bytes: usize) -> String {
    let body = task_text(task);
    if body.is_empty() {
        return String::new();
    }
    quoted(agent, &body, max_output_bytes)
}

/// A direct reply's text between the delimiters.
///
/// Always framed, even when the message carries no text: a reply is the
/// agent's answer, and an empty one between delimiters says so where an
/// absent block would leave the reader wondering whether there was one.
pub fn remote_reply(agent: &str, message: &Message, max_output_bytes: usize) -> String {
    quoted(agent, &parts_text(&message.parts), max_output_bytes)
}

/// What an agent's card says it does — its description and skills — bounded
/// for a tool description, with `…` when anything was cut.
///
/// Empty when the card says nothing about itself. Every line ends in a
/// newline, so the summary composes between other lines unchanged.
pub fn card_summary(card: &AgentCard) -> String {
    let mut about = String::new();
    let own = card.description.trim();
    if !own.is_empty() {
        about.push_str(&format!("It describes itself as: {own}\n"));
    }
    let skills: Vec<String> = card
        .skills
        .iter()
        .take(MAX_SKILLS)
        .map(|skill| {
            let (text, _) = clip(skill.description.trim(), MAX_SKILL_TEXT_BYTES);
            format!("- {}: {text}", skill.name.trim())
        })
        .collect();
    if !skills.is_empty() {
        about.push_str("Its skills:\n");
        about.push_str(&skills.join("\n"));
        about.push('\n');
        let more = card.skills.len().saturating_sub(MAX_SKILLS);
        if more > 0 {
            about.push_str(&format!("- and {more} more\n"));
        }
    }
    let (shown, omitted) = clip(&about, MAX_CARD_TEXT_BYTES);
    let mut summary = shown.to_string();
    if omitted > 0 {
        summary.push_str("…\n");
    }
    summary
}

/// A remote agent's words as one line for a progress report: every run of
/// whitespace or control characters — newlines and terminal escapes
/// included — becomes one space, and the line is clipped to `max_bytes`, with
/// `…` when anything was cut.
///
/// A progress line is not framed, so it must not be able to look like more
/// than one line, or like anything but the agent's words.
pub(crate) fn remote_line(text: &str, max_bytes: usize) -> String {
    let line = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match clip(&line, max_bytes) {
        (shown, 0) => shown.to_string(),
        (shown, _) => format!("{shown}…"),
    }
}

/// The text of a task: its artifacts, and its status message when that is
/// where the agent put its words.
fn task_text(task: &Task) -> String {
    let artifacts = task.artifacts.as_deref().unwrap_or_default();
    let mut sections = Vec::new();

    let status_message = task
        .status
        .message
        .as_ref()
        .filter(|_| task.status.state != TaskState::Completed || artifacts.is_empty())
        .map(|message| parts_text(&message.parts))
        .filter(|text| !text.is_empty());
    sections.extend(status_message);

    let labelled = artifacts.len() > 1;
    for artifact in artifacts {
        let body = parts_text(&artifact.parts);
        let label = artifact
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty());
        match label {
            Some(name) => sections.push(format!("[artifact: {name}]\n{body}")),
            None if labelled => {
                sections.push(format!("[artifact: {}]\n{body}", artifact.artifact_id))
            }
            None => sections.push(body),
        }
    }

    sections.join("\n\n")
}

/// The text of a list of parts, with every non-text part summarized.
fn parts_text(parts: &[Part]) -> String {
    parts.iter().map(part_text).collect::<Vec<_>>().join("\n")
}

/// One part as text: text itself, anything else as a one-line summary.
fn part_text(part: &Part) -> String {
    let mut about = String::new();
    if let Some(filename) = &part.filename {
        about.push_str(&format!(" {filename}"));
    }
    if let Some(media_type) = &part.media_type {
        about.push_str(&format!(" ({media_type})"));
    }
    match &part.content {
        PartContent::Text(text) => text.clone(),
        PartContent::Raw(bytes) => {
            format!("[file{about}: {} bytes inline, not shown]", bytes.len())
        }
        PartContent::Url(url) => format!("[file{about} at {url}]"),
        PartContent::Data(value) => format!("[data{about}]\n{value}"),
    }
}

/// The agent's own words between delimiters, clipped to the output bound.
fn quoted(agent: &str, body: &str, max_output_bytes: usize) -> String {
    let (shown, omitted) = clip(body, max_output_bytes);
    let mut text = format!("--- begin output from remote agent `{agent}` ---\n");
    text.push_str(shown);
    if omitted > 0 {
        text.push_str(&format!("\n[truncated: {omitted} more bytes not shown]"));
    }
    text.push_str(&format!("\n--- end output from remote agent `{agent}` ---"));
    text
}

/// The longest prefix of `text` within `max_bytes` that ends on a character
/// boundary, and how many bytes were left out.
fn clip(text: &str, max_bytes: usize) -> (&str, usize) {
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}
