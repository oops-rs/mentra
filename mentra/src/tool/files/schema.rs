use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct FilesInput {
    #[serde(rename = "workingDirectory")]
    pub(crate) working_directory: Option<String>,
    pub(crate) operations: Vec<FileOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum FileOperation {
    Read {
        path: String,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    List {
        path: String,
        depth: Option<usize>,
        limit: Option<usize>,
    },
    Search {
        path: String,
        pattern: String,
        limit: Option<usize>,
    },
    Create {
        path: String,
        content: String,
    },
    Set {
        path: String,
        content: String,
    },
    Replace {
        path: String,
        old: String,
        new: String,
        #[serde(rename = "replaceAll")]
        replace_all: Option<bool>,
        #[serde(rename = "expectedReplacements")]
        expected_replacements: Option<usize>,
    },
    Insert {
        path: String,
        anchor: String,
        position: InsertPosition,
        content: String,
        occurrence: Option<usize>,
    },
    Move {
        from: String,
        to: String,
    },
    Delete {
        path: String,
    },
}

impl FileOperation {
    pub(crate) fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Read { .. } | Self::List { .. } | Self::Search { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InsertPosition {
    Before,
    After,
}
