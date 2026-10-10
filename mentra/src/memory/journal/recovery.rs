//! Telling a finished turn apart from one a crash interrupted (#64).
//!
//! A run is finished only when its last committed message is an assistant
//! message with no unresolved tool calls. Anything else stopped inside the
//! turn: tool calls whose results never landed, or results the model never
//! got to answer. Recovery closes the unresolved calls rather than running
//! them again, because a call that was executing when the process died may
//! already have had its effect.

use std::collections::BTreeSet;

use crate::{ContentBlock, Message, Role};

/// What the result of a call that was running when the process stopped says.
///
/// Addressed to the model, which reads it in place of the call's real
/// result: it must not assume the call failed, nor that it worked.
pub(crate) const INTERRUPTED_TOOL_CALL_RESULT: &str = "The process stopped while this tool call \
     was running, so its effect is unknown: it may have completed, partly run, or not run at all. \
     It was not retried. Check the current state before repeating it.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecoveryOutcome {
    pub interrupted: bool,
    pub interrupted_run_id: Option<String>,
}

/// Where a committed transcript stands with respect to its last turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnState {
    /// The last message is an assistant message whose every tool call, if
    /// any, has a result.
    Finished,
    /// The turn stopped before the model answered. `unresolved` names the
    /// calls of the last assistant message that have no result, in call
    /// order; empty when every call was answered and only the model's reply
    /// is missing.
    Interrupted { unresolved: Vec<String> },
}

impl TurnState {
    /// Classifies a transcript by its projected messages.
    pub(crate) fn of(history: &[Message]) -> Self {
        let Some(index) = history
            .iter()
            .rposition(|message| message.role == Role::Assistant)
        else {
            return Self::Interrupted {
                unresolved: Vec::new(),
            };
        };
        let answered: BTreeSet<&str> = history[index + 1..]
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        let unresolved: Vec<String> = history[index]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } if !answered.contains(id.as_str()) => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        if unresolved.is_empty() && index + 1 == history.len() {
            Self::Finished
        } else {
            Self::Interrupted { unresolved }
        }
    }
}

/// The user message that closes `unresolved` calls with an error result
/// each, or `None` when there is nothing to close.
pub(crate) fn closing_message(unresolved: &[String]) -> Option<Message> {
    if unresolved.is_empty() {
        return None;
    }
    Some(Message {
        role: Role::User,
        content: unresolved
            .iter()
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: INTERRUPTED_TOOL_CALL_RESULT.to_string().into(),
                is_error: true,
            })
            .collect(),
    })
}
