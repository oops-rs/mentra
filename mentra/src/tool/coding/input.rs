use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tool::files::workspace::{SearchOptions, TextEdit};

#[derive(Debug, Deserialize)]
pub(super) struct ReadInput {
    #[serde(alias = "file_path", alias = "filePath")]
    pub(super) path: String,
    pub(super) offset: Option<usize>,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ListInput {
    #[serde(default)]
    pub(super) path: Option<String>,
    pub(super) depth: Option<usize>,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GrepInput {
    pub(super) pattern: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) glob: Option<String>,
    #[serde(default, alias = "ignoreCase")]
    pub(super) ignore_case: bool,
    #[serde(default)]
    pub(super) literal: bool,
    #[serde(default)]
    pub(super) context: usize,
    #[serde(default)]
    pub(super) multiline: bool,
    pub(super) limit: Option<usize>,
}

impl GrepInput {
    pub(super) fn search_options(&self) -> SearchOptions {
        SearchOptions {
            file_glob: self.glob.clone(),
            ignore_case: self.ignore_case,
            literal: self.literal,
            context: self.context,
            multiline: self.multiline,
            max_line_chars: Some(500),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct GlobInput {
    pub(super) pattern: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WriteInput {
    #[serde(alias = "file_path", alias = "filePath")]
    pub(super) path: String,
    pub(super) content: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct EditInput {
    #[serde(alias = "file_path", alias = "filePath")]
    pub(super) path: String,
    pub(super) edits: Vec<EditSpec>,
    #[serde(default, alias = "replaceAll")]
    pub(super) replace_all: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct EditSpec {
    #[serde(alias = "oldText", alias = "old")]
    old_string: String,
    #[serde(alias = "newText", alias = "new")]
    new_string: String,
}

impl From<EditSpec> for TextEdit {
    fn from(value: EditSpec) -> Self {
        Self {
            old_string: value.old_string,
            new_string: value.new_string,
        }
    }
}

pub(super) fn parse_read(input: Value) -> Result<ReadInput, String> {
    parse(input, "read")
}

pub(super) fn parse_list(input: Value) -> Result<ListInput, String> {
    parse(input, "ls")
}

pub(super) fn parse_grep(input: Value) -> Result<GrepInput, String> {
    parse(input, "grep")
}

pub(super) fn parse_glob(input: Value) -> Result<GlobInput, String> {
    parse(input, "glob")
}

pub(super) fn parse_write(input: Value) -> Result<WriteInput, String> {
    parse(input, "write")
}

pub(super) fn parse_edit(input: Value) -> Result<EditInput, String> {
    let mut object = input
        .as_object()
        .cloned()
        .ok_or_else(|| "Invalid edit input: expected an object".to_string())?;
    normalize_edits(&mut object)?;
    parse(Value::Object(object), "edit")
}

fn normalize_edits(object: &mut Map<String, Value>) -> Result<(), String> {
    if let Some(Value::String(encoded)) = object.get("edits") {
        let decoded: Value = serde_json::from_str(encoded)
            .map_err(|error| format!("Invalid edit input: edits JSON string: {error}"))?;
        object.insert("edits".to_string(), normalize_edit_collection(decoded)?);
    } else if let Some(edits) = object.get("edits").cloned() {
        object.insert("edits".to_string(), normalize_edit_collection(edits)?);
    } else {
        let old = take_first(object, &["old_string", "oldText", "old"]);
        let new = take_first(object, &["new_string", "newText", "new"]);
        if old.is_some() || new.is_some() {
            let mut edit = Map::new();
            if let Some(old) = old {
                edit.insert("old_string".to_string(), old);
            }
            if let Some(new) = new {
                edit.insert("new_string".to_string(), new);
            }
            object.insert("edits".to_string(), Value::Array(vec![Value::Object(edit)]));
        }
    }
    Ok(())
}

fn normalize_edit_collection(value: Value) -> Result<Value, String> {
    match value {
        Value::Array(_) => Ok(value),
        Value::Object(_) => Ok(Value::Array(vec![value])),
        _ => Err("Invalid edit input: edits must be an array, object, or JSON string".to_string()),
    }
}

fn take_first(object: &mut Map<String, Value>, keys: &[&str]) -> Option<Value> {
    keys.iter().find_map(|key| object.remove(*key))
}

fn parse<T>(input: Value, tool: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(input).map_err(|error| format!("Invalid {tool} input: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn edit_accepts_json_encoded_edits_and_camel_case_aliases() {
        let parsed = parse_edit(json!({
            "filePath": "src/lib.rs",
            "edits": r#"[{"oldText":"before","newText":"after"}]"#,
            "replaceAll": true
        }))
        .expect("parse edit");

        assert_eq!(parsed.path, "src/lib.rs");
        assert!(parsed.replace_all);
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].old_string, "before");
        assert_eq!(parsed.edits[0].new_string, "after");
    }

    #[test]
    fn edit_accepts_legacy_top_level_single_edit() {
        let parsed = parse_edit(json!({
            "file_path": "src/lib.rs",
            "old_string": "before",
            "new_string": "after"
        }))
        .expect("parse edit");

        assert_eq!(parsed.path, "src/lib.rs");
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].old_string, "before");
        assert_eq!(parsed.edits[0].new_string, "after");
    }
}
