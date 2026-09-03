//! Filesystem primitives the file store's durability model rests on:
//! atomic replace via temp-file-and-rename, fsynced appends that always
//! start on a fresh line, and a conservative directory-name encoding.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::runtime::error::RuntimeError;

use super::store_error;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Opens a stable sidecar file suitable for an advisory lock.
///
/// Lock files are never unlinked: replacing or removing one while another
/// process waits on its inode can let two callers hold locks on different
/// files for the same logical resource.
pub(super) fn open_lock_file(path: &Path) -> Result<File, RuntimeError> {
    let parent = parent_dir(path)?;
    fs::create_dir_all(parent)
        .map_err(|error| store_error(&format!("create '{}'", parent.display()), error))?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| store_error(&format!("open '{}'", path.display()), error))
}

/// Opens `path` and holds its exclusive advisory lock until the returned
/// handle is dropped.
pub(super) fn lock_exclusive(path: &Path) -> Result<File, RuntimeError> {
    let file = open_lock_file(path)?;
    fs2::FileExt::lock_exclusive(&file)
        .map_err(|error| store_error(&format!("lock '{}'", path.display()), error))?;
    Ok(file)
}

/// Replaces `path` atomically: write a temp file beside it, fsync, rename
/// over the target, fsync the parent directory. A reader never observes a
/// partial file, and a crash leaves at most an ignored temp file behind.
pub(super) fn atomic_replace(path: &Path, contents: &[u8]) -> Result<(), RuntimeError> {
    let parent = parent_dir(path)?;
    fs::create_dir_all(parent)
        .map_err(|error| store_error(&format!("create '{}'", parent.display()), error))?;

    let temp_path = parent.join(format!(
        ".{}.tmp-{}-{}",
        file_name(path)?,
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed),
    ));
    let write = (|| -> std::io::Result<()> {
        let mut file = File::create(&temp_path)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&temp_path);
        return Err(store_error(
            &format!("write '{}'", temp_path.display()),
            error,
        ));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(store_error(
            &format!("rename into '{}'", path.display()),
            error,
        ));
    }
    fsync_dir(parent)
}

/// Appends `lines` (each without a trailing newline) to `path`, creating it
/// if needed, and fsyncs before returning. If the file's current tail is a
/// truncated line — an append cut short by a crash, whose write was never
/// acknowledged — the tail is first cut back to the last complete line, so
/// every append starts on a fresh line and the log only ever carries whole
/// ones.
pub(super) fn append_lines(path: &Path, lines: &[String]) -> Result<(), RuntimeError> {
    if lines.is_empty() {
        return Ok(());
    }
    let parent = parent_dir(path)?;
    fs::create_dir_all(parent)
        .map_err(|error| store_error(&format!("create '{}'", parent.display()), error))?;

    let existed = path.exists();
    let mut buffer = String::new();
    for line in lines {
        buffer.push_str(line);
        buffer.push('\n');
    }

    let append = (|| -> std::io::Result<()> {
        // The tail repair uses its own read+write handle: on Windows an
        // append-mode handle carries FILE_APPEND_DATA without
        // FILE_WRITE_DATA, so set_len on it fails. The append itself then
        // goes through a plain append handle, whose writes stay atomic at
        // the end of the file.
        if existed {
            let mut repair = OpenOptions::new().read(true).write(true).open(path)?;
            drop_truncated_tail(&mut repair)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(buffer.as_bytes())?;
        file.sync_all()
    })();
    append.map_err(|error| store_error(&format!("append to '{}'", path.display()), error))?;
    if !existed {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Truncates `file` back to just past its last newline when its final line
/// is incomplete. The dropped bytes belong to a write that never finished,
/// so no reader ever saw them as committed. `file` must be open for both
/// reading and writing — not appending — or `set_len` fails on Windows.
fn drop_truncated_tail(file: &mut File) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }

    const CHUNK: u64 = 4096;
    let mut end = len;
    loop {
        let start = end.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if end == len && chunk.last() == Some(&b'\n') {
            return Ok(());
        }
        if let Some(offset) = chunk.iter().rposition(|byte| *byte == b'\n') {
            file.set_len(start + offset as u64 + 1)?;
            return Ok(());
        }
        if start == 0 {
            file.set_len(0)?;
            return Ok(());
        }
        end = start;
    }
}

/// Reads a file that may not exist yet; `Ok(None)` when it does not.
pub(super) fn read_optional(path: &Path) -> Result<Option<String>, RuntimeError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(store_error(&format!("read '{}'", path.display()), error)),
    }
}

/// Flushes a directory so a rename or file creation inside it is durable.
/// Directories cannot be opened for syncing on Windows; the rename itself is
/// still atomic there, which is the property correctness relies on.
pub(super) fn fsync_dir(dir: &Path) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        let handle = File::open(dir)
            .map_err(|error| store_error(&format!("open directory '{}'", dir.display()), error))?;
        handle
            .sync_all()
            .map_err(|error| store_error(&format!("sync directory '{}'", dir.display()), error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Maps an identifier onto a filesystem-safe directory name, injectively
/// even on case-insensitive filesystems: an id that is already a tame
/// lowercase filename is used as-is; everything else becomes `x-` plus its
/// bytes in lowercase hex.
///
/// Injectivity: two plain ids are distinct strings over an alphabet with
/// one spelling per character, so they never collide even where the
/// filesystem folds case; two encoded ids collide only if their bytes do;
/// and the forms never cross, because a plain id is barred from starting
/// with `x-`. Mixed case routes to the encoding for exactly that reason —
/// `Agent` and `agent` must not share a directory on macOS or Windows.
pub(super) fn encode_component(id: &str) -> String {
    if is_plain_component(id) {
        return id.to_string();
    }
    let mut encoded = String::with_capacity(2 + id.len() * 2);
    encoded.push_str("x-");
    for byte in id.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

fn is_plain_component(id: &str) -> bool {
    if id.is_empty() || id.starts_with('.') || id.ends_with('.') || id.starts_with("x-") {
        return false;
    }
    let tame = id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'));
    if !tame {
        return false;
    }
    // Windows reserves device names regardless of extension: `con` and
    // `con.log` both name the console.
    let stem = id.split('.').next().unwrap_or(id);
    !is_windows_reserved_device(stem)
}

fn is_windows_reserved_device(stem: &str) -> bool {
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn parent_dir(path: &Path) -> Result<&Path, RuntimeError> {
    path.parent().ok_or_else(|| {
        RuntimeError::Store(format!("path '{}' has no parent directory", path.display()))
    })
}

fn file_name(path: &Path) -> Result<&str, RuntimeError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            RuntimeError::Store(format!("path '{}' has no usable file name", path.display()))
        })
}
