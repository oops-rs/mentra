use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

#[cfg(not(test))]
use directories::BaseDirs;

const APP_DIR_NAME: &str = "mentra";
const WORKSPACES_DIR_NAME: &str = "workspaces";
const TEAM_DIR_NAME: &str = "team";
const TASKS_DIR_NAME: &str = "tasks";
const TRANSCRIPTS_DIR_NAME: &str = "transcripts";
const FALLBACK_DIR_NAME: &str = ".mentra";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceDefaultPaths {
    pub(crate) root_dir: PathBuf,
    pub(crate) default_store_path: PathBuf,
    pub(crate) team_dir: PathBuf,
    pub(crate) tasks_dir: PathBuf,
    pub(crate) transcripts_dir: PathBuf,
}

#[cfg(not(test))]
pub(crate) fn workspace_default_paths() -> WorkspaceDefaultPaths {
    workspace_default_paths_for(canonical_workspace_dir(), platform_data_local_dir())
}

pub(crate) fn workspace_default_paths_for(
    workspace_dir: PathBuf,
    data_local_dir: Option<PathBuf>,
) -> WorkspaceDefaultPaths {
    let workspace_dir = canonicalize_or_original(workspace_dir);
    let workspace_hash = workspace_hash(&workspace_dir);
    let root_dir = match data_local_dir {
        Some(data_local_dir) => data_local_dir
            .join(APP_DIR_NAME)
            .join(WORKSPACES_DIR_NAME)
            .join(workspace_hash),
        None => workspace_dir
            .join(FALLBACK_DIR_NAME)
            .join(WORKSPACES_DIR_NAME)
            .join(workspace_hash),
    };

    WorkspaceDefaultPaths {
        default_store_path: root_dir.join("runtime.sqlite"),
        team_dir: root_dir.join(TEAM_DIR_NAME),
        tasks_dir: root_dir.join(TASKS_DIR_NAME),
        transcripts_dir: root_dir.join(TRANSCRIPTS_DIR_NAME),
        root_dir,
    }
}

#[cfg(not(test))]
fn platform_data_local_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.data_local_dir().to_path_buf())
}

#[cfg(not(test))]
fn canonical_workspace_dir() -> PathBuf {
    canonicalize_or_original(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn workspace_hash(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join("mentra-default-paths-tests")
            .join(label)
    }

    #[test]
    fn uses_platform_data_directory_when_available() {
        let workspace = test_path("release-check-workspace");
        let data_dir = test_path("release-check-data");

        let paths = workspace_default_paths_for(workspace.clone(), Some(data_dir.clone()));

        assert!(
            paths
                .root_dir
                .starts_with(data_dir.join(APP_DIR_NAME).join(WORKSPACES_DIR_NAME))
        );
        assert!(paths.root_dir.ends_with(workspace_hash(&workspace)));
        assert_eq!(
            paths.default_store_path,
            paths.root_dir.join("runtime.sqlite")
        );
        assert_eq!(paths.team_dir, paths.root_dir.join(TEAM_DIR_NAME));
        assert_eq!(paths.tasks_dir, paths.root_dir.join(TASKS_DIR_NAME));
        assert_eq!(
            paths.transcripts_dir,
            paths.root_dir.join(TRANSCRIPTS_DIR_NAME)
        );
    }

    #[test]
    fn falls_back_to_workspace_dot_directory_without_platform_data_dir() {
        let workspace = test_path("fallback-check-workspace");

        let paths = workspace_default_paths_for(workspace.clone(), None);

        assert_eq!(
            paths.root_dir,
            workspace
                .join(FALLBACK_DIR_NAME)
                .join(WORKSPACES_DIR_NAME)
                .join(workspace_hash(&workspace))
        );
    }

    #[test]
    fn same_workspace_produces_shared_root_for_all_default_paths() {
        let workspace = test_path("shared-root-workspace");
        let data_dir = test_path("shared-root-data");

        let paths = workspace_default_paths_for(workspace, Some(data_dir));

        for derived_path in [
            &paths.default_store_path,
            &paths.team_dir,
            &paths.tasks_dir,
            &paths.transcripts_dir,
        ] {
            assert!(derived_path.starts_with(&paths.root_dir));
        }
    }
}
