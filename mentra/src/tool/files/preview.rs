use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::tool::{ParallelToolContext, RuntimeToolDescriptor, ToolAuthorizationPreview};

use super::{
    input::{ensure_files_have_operations, parse_files_input},
    schema::{self, FileOperation},
};

pub(crate) fn build_files_authorization_preview(
    descriptor: RuntimeToolDescriptor,
    ctx: &ParallelToolContext,
    input: &Value,
) -> Result<ToolAuthorizationPreview, String> {
    let raw_input = input.clone();
    let input = parse_files_input(input)?;
    ensure_files_have_operations(&input)?;

    let working_directory = match input.working_directory.as_deref() {
        Some(directory) => ctx.resolve_working_directory(Some(directory))?,
        None => ctx
            .runtime
            .resolve_working_directory(&ctx.agent_id, None)
            .unwrap_or_else(|_| ctx.working_directory().to_path_buf()),
    };

    let operations = input
        .operations
        .into_iter()
        .map(|operation| preview_file_operation(&working_directory, operation))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ToolAuthorizationPreview {
        working_directory: working_directory.clone(),
        capabilities: descriptor.capabilities,
        side_effect_level: descriptor.side_effect_level,
        durability: descriptor.durability,
        execution_category: descriptor.execution_category,
        approval_category: descriptor.approval_category,
        raw_input,
        structured_input: json!({
            "kind": "files",
            "working_directory": working_directory,
            "operations": operations,
        }),
    })
}

fn preview_file_operation(
    working_directory: &Path,
    operation: FileOperation,
) -> Result<Value, String> {
    match operation {
        FileOperation::Read {
            path,
            offset,
            limit,
        } => Ok(json!({
            "op": "read",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "offset": offset,
            "limit": limit,
        })),
        FileOperation::List { path, depth, limit } => Ok(json!({
            "op": "list",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "depth": depth,
            "limit": limit,
        })),
        FileOperation::Search {
            path,
            pattern,
            limit,
        } => Ok(json!({
            "op": "search",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "pattern": pattern,
            "limit": limit,
        })),
        FileOperation::Create { path, .. } => Ok(json!({
            "op": "create",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
        FileOperation::Set { path, .. } => Ok(json!({
            "op": "set",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
        FileOperation::Replace {
            path,
            replace_all,
            expected_replacements,
            ..
        } => Ok(json!({
            "op": "replace",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "replace_all": replace_all,
            "expected_replacements": expected_replacements,
        })),
        FileOperation::Insert {
            path,
            position,
            occurrence,
            ..
        } => Ok(json!({
            "op": "insert",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
            "position": match position {
                schema::InsertPosition::Before => "before",
                schema::InsertPosition::After => "after",
            },
            "occurrence": occurrence,
        })),
        FileOperation::Move { from, to } => Ok(json!({
            "op": "move",
            "from_resolved_path": resolve_preview_path(working_directory, &from)?,
            "to_resolved_path": resolve_preview_path(working_directory, &to)?,
        })),
        FileOperation::Delete { path } => Ok(json!({
            "op": "delete",
            "resolved_path": resolve_preview_path(working_directory, &path)?,
        })),
    }
}

fn resolve_preview_path(working_directory: &Path, raw: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(raw);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        working_directory.join(candidate)
    };
    normalize_preview_path(path)
}

fn normalize_preview_path(path: PathBuf) -> Result<PathBuf, String> {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    };

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.is_absolute() {
                    return Err(format!(
                        "Path '{}' escapes the filesystem root",
                        path.display()
                    ));
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    if !normalized.is_absolute() {
        return Err(format!(
            "Path '{}' must resolve to an absolute path",
            path.display()
        ));
    }

    Ok(normalized)
}
