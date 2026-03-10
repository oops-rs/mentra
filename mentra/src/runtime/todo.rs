use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const TODO_TOOL_NAME: &str = "todo";
pub(crate) const TODO_REMINDER_THRESHOLD: usize = 3;
pub(crate) const TODO_REMINDER_TEXT: &str =
    "Reminder: update your todo list with todo before continuing multi-step work.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    items: Vec<TodoItem>,
}

pub(crate) fn parse_todo_input(input: Value) -> Result<Vec<TodoItem>, String> {
    let parsed = serde_json::from_value::<TodoWriteInput>(input)
        .map_err(|error| format!("Invalid todo input: {error}"))?;
    validate_todos(&parsed.items)?;
    Ok(parsed.items)
}

pub(crate) fn validate_todos(items: &[TodoItem]) -> Result<(), String> {
    let mut seen_ids = HashSet::new();
    let mut in_progress_count = 0;

    for item in items {
        if item.id.trim().is_empty() {
            return Err("Todo item id must not be empty".to_string());
        }

        if item.text.trim().is_empty() {
            return Err(format!("Todo item '{}' text must not be empty", item.id));
        }

        if !seen_ids.insert(item.id.as_str()) {
            return Err(format!("Duplicate todo item id '{}'", item.id));
        }

        if item.status == TodoStatus::InProgress {
            in_progress_count += 1;
            if in_progress_count > 1 {
                return Err("Only one todo item can be in_progress".to_string());
            }
        }
    }

    Ok(())
}

pub(crate) fn has_unfinished_todos(items: &[TodoItem]) -> bool {
    items
        .iter()
        .any(|item| item.status != TodoStatus::Completed)
}

pub(crate) fn render_todos(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "No todos.".to_string();
    }

    items
        .iter()
        .map(|item| {
            let marker = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[>]",
                TodoStatus::Completed => "[x]",
            };
            format!("{marker} {}: {}", item.id, item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
