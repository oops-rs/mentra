//! A file a tool reads or writes on an agent's behalf, under the same policy
//! as the file tools.
//!
//! A tool that sends a workspace file somewhere, or saves one it was handed,
//! touches the filesystem for the model as surely as `read` and `write` do.
//! Going around the runtime's read and write roots would let it reach what
//! the file tools refuse, so these resolve a path the way the editor does —
//! against the agent's working directory, normalized — authorize it against
//! the same roots, and report a refusal as the same `AuthorizationDenied`
//! hook event.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::workspace::normalize_path;
use crate::runtime::{RuntimeHandle, RuntimeHookEvent};

/// The agent's own view of where it is: what the editor resolves against.
fn working_directory(runtime: &RuntimeHandle, agent_id: &str, fallback: &Path) -> PathBuf {
    runtime
        .resolve_working_directory(agent_id, None)
        .unwrap_or_else(|_| fallback.to_path_buf())
}

fn resolve(
    runtime: &RuntimeHandle,
    agent_id: &str,
    fallback: &Path,
    raw: &str,
) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(raw);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        working_directory(runtime, agent_id, fallback).join(candidate)
    };
    normalize_path(path)
}

fn denied(runtime: &RuntimeHandle, agent_id: &str, action: &str, detail: &str) {
    let _ = runtime.emit_hook(RuntimeHookEvent::AuthorizationDenied {
        agent_id: agent_id.to_string(),
        action: action.to_string(),
        detail: detail.to_string(),
    });
}

/// Reads `raw` for `agent_id`: its resolved path and its bytes.
pub(crate) fn read(
    runtime: &RuntimeHandle,
    agent_id: &str,
    fallback: &Path,
    raw: &str,
) -> Result<(PathBuf, Vec<u8>), String> {
    let base_dir = runtime.agent_config(agent_id)?.base_dir;
    let path = resolve(runtime, agent_id, fallback, raw)?;
    let path = runtime
        .execution
        .policy
        .authorize_file_read(&base_dir, &path)
        .inspect_err(|detail| denied(runtime, agent_id, "files_read", detail))?;
    let bytes = fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("Path '{raw}' does not exist"),
        _ => format!("Failed to read '{raw}': {error}"),
    })?;
    Ok((path, bytes))
}

/// Writes `bytes` to `raw` for `agent_id`, making its directories: the
/// resolved path written.
pub(crate) fn write(
    runtime: &RuntimeHandle,
    agent_id: &str,
    fallback: &Path,
    raw: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let base_dir = runtime.agent_config(agent_id)?.base_dir;
    let path = resolve(runtime, agent_id, fallback, raw)?;
    let path = runtime
        .execution
        .policy
        .authorize_file_write(&base_dir, &path)
        .inspect_err(|detail| denied(runtime, agent_id, "files_write", detail))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create '{}': {error}", parent.display()))?;
    }
    fs::write(&path, bytes).map_err(|error| format!("Failed to write '{raw}': {error}"))?;
    Ok(path)
}
