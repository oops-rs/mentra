use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use mentra::runtime::{RuntimePolicy, SessionOptions, SessionResumeOptions, normalize_policy_root};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mentra-policy-api-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn session_policy_options_are_public_and_default_to_runtime_inheritance() {
    assert!(SessionOptions::default().policy.is_none());
    assert!(SessionResumeOptions::default().policy.is_none());

    let create = SessionOptions {
        policy: Some(RuntimePolicy::default()),
        ..Default::default()
    };
    let resume = SessionResumeOptions {
        policy: Some(RuntimePolicy::default()),
        ..Default::default()
    };

    assert!(create.policy.is_some());
    assert!(resume.policy.is_some());
}

#[test]
fn public_policy_root_normalizer_folds_lexical_traversal() {
    let directory = TestDirectory::new("traversal");
    fs::create_dir_all(directory.path().join("one")).expect("create traversal component");
    fs::create_dir_all(directory.path().join("two")).expect("create traversal target");

    let traversed = directory.path().join("one").join("..").join("two");
    let direct = directory.path().join("two");

    assert_eq!(
        normalize_policy_root(&traversed),
        normalize_policy_root(&direct)
    );
}

#[test]
fn public_policy_root_normalizer_preserves_a_missing_tail() {
    let directory = TestDirectory::new("missing-tail");
    let tail = Path::new("not-created").join("deeper").join("file.txt");
    let candidate = directory.path().join(&tail);

    assert!(!candidate.exists(), "the test tail must stay absent");
    assert_eq!(
        normalize_policy_root(&candidate),
        normalize_policy_root(directory.path()).join(tail)
    );
}

#[test]
fn public_policy_root_normalizer_matches_the_filesystem_canonical_spelling() {
    let directory = TestDirectory::new("canonical-spelling");
    let missing_tail = Path::new("not-created").join("file.txt");
    let ordinary = directory.path().join(&missing_tail);
    let canonical = fs::canonicalize(directory.path())
        .expect("canonicalize test directory")
        .join(missing_tail);

    assert_eq!(
        normalize_policy_root(&ordinary),
        normalize_policy_root(&canonical)
    );
}

#[cfg(unix)]
#[test]
fn public_policy_root_normalizer_resolves_an_existing_symlink_prefix() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("symlink-prefix");
    let target = directory.path().join("target");
    let child = target.join("child");
    fs::create_dir_all(&child).expect("create symlink target");
    let link = directory.path().join("link");
    symlink(&target, &link).expect("create directory symlink");

    assert_eq!(
        normalize_policy_root(&link.join("child")),
        normalize_policy_root(&child)
    );
}
