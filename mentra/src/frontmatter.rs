//! Markdown frontmatter: splitting a file at its `---` delimiters, and
//! reading the flat `key: value` block between them.
//!
//! The delimiter rules are markdown frontmatter's: a leading `---` line
//! opens, the next bare `---` line closes, and everything after it is the
//! body. Three conventions read this shape — `SKILL.md` here, prompt
//! templates and memory files in basis — and the scanner lives once so they
//! cannot drift on delimiters, byte-order marks, or line endings.
//!
//! What the block holds is read by [`parse`], which understands the YAML
//! those conventions actually use and nothing more: one `key: value` per
//! line, a value plain or quoted, `true`/`false`, blank lines and `#`
//! comments. A list, a nested mapping or a block scalar is refused with a
//! message that says so. Every field those conventions define is a string
//! or a boolean, so a full YAML parser bought nothing but an unmaintained
//! C-library port underneath it; what a writer needs is [`quote`], which
//! produces a value [`parse`] reads back exactly.
//!
//! What a field *means* — which keys, which are required, which file to
//! blame — stays with each caller.

use std::fmt;

/// A file split at its frontmatter delimiters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned<'a> {
    /// The raw block between the delimiters, or `None` when the file never
    /// opened frontmatter and is all body.
    pub frontmatter: Option<&'a str>,
    pub body: &'a str,
}

/// A file that opened frontmatter and never closed it — an error, not a file
/// whose body happens to start with `---`. Carries no path: the caller is the
/// one holding a file name to blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unterminated;

impl fmt::Display for Unterminated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("missing closing frontmatter delimiter")
    }
}

impl std::error::Error for Unterminated {}

/// Splits `raw` into its frontmatter and the body below it.
///
/// A file with no frontmatter is not an error here — it is all body. Whether
/// that is *usable* is the caller's question. Empty frontmatter (`---\n---`)
/// scans as `Some("")`, distinguishable from a file that never opened one.
pub fn scan(raw: &str) -> Result<Scanned<'_>, Unterminated> {
    // A byte-order mark would keep the `---` from starting the file, turning
    // frontmatter into prose and the failure into a confusing one.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);

    let Some(opening) = raw
        .strip_prefix("---\r\n")
        .or_else(|| raw.strip_prefix("---\n"))
    else {
        return Ok(Scanned {
            frontmatter: None,
            body: raw,
        });
    };

    let mut cursor = 0usize;
    for segment in opening.split_inclusive('\n') {
        if segment.trim_end_matches(['\n', '\r']) == "---" {
            return Ok(Scanned {
                frontmatter: Some(&opening[..cursor]),
                body: &opening[cursor + segment.len()..],
            });
        }
        cursor += segment.len();
    }

    // The closing delimiter can also be the last line, with no newline after
    // it; then the body is empty rather than missing.
    if opening[cursor..].trim_end_matches('\r') == "---" {
        return Ok(Scanned {
            frontmatter: Some(&opening[..cursor]),
            body: "",
        });
    }

    Err(Unterminated)
}

/// One value in a frontmatter block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    /// A string, plain or quoted.
    Text(String),
    /// A plain `true` or `false`.
    Bool(bool),
    /// `key:` with nothing after it, or a plain `null` / `~`.
    Null,
}

/// The fields of a frontmatter block, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    entries: Vec<(String, Scalar)>,
}

impl Fields {
    /// The value under `key`, if the block has it.
    pub fn get(&self, key: &str) -> Option<&Scalar> {
        self.entries
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| value)
    }

    /// The string under `key`: a text value as written, a boolean as
    /// `true`/`false`, `None` for an absent key or a null.
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Scalar::Text(text) => Some(text),
            Scalar::Bool(true) => Some("true"),
            Scalar::Bool(false) => Some("false"),
            Scalar::Null => None,
        }
    }

    /// The boolean under `key`, or `None` when absent or null. A text
    /// value spelled `true`/`false` (quoted, say) counts; anything else is
    /// an error naming the key.
    pub fn boolean(&self, key: &str) -> Result<Option<bool>, ParseError> {
        match self.get(key) {
            None | Some(Scalar::Null) => Ok(None),
            Some(Scalar::Bool(value)) => Ok(Some(*value)),
            Some(Scalar::Text(text)) => match text.as_str() {
                "true" => Ok(Some(true)),
                "false" => Ok(Some(false)),
                other => Err(ParseError {
                    line: 0,
                    message: format!("`{key}` must be true or false, not `{other}`"),
                }),
            },
        }
    }

    /// The first of `keys` the block has, for a field with two spellings.
    pub fn text_any<'a>(&'a self, keys: &[&str]) -> Option<&'a str> {
        keys.iter().find_map(|key| self.text(key))
    }

    /// The first of `keys` the block has, as a boolean.
    pub fn boolean_any(&self, keys: &[&str]) -> Result<Option<bool>, ParseError> {
        for key in keys {
            if let Some(value) = self.boolean(key)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every key, in file order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }
}

/// A block [`parse`] could not read, with the line it stopped at (1-based;
/// 0 when the fault is in a value rather than a line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ParseError {}

/// Reads a frontmatter block: one `key: value` per line.
///
/// A key is letters, digits, `_` and `-`. A value is plain (trimmed, up to a
/// ` #` comment), double-quoted with `\"`, `\\`, `\n`, `\t`, `\r` and
/// `\uXXXX` escapes, or single-quoted with `''` for a quote. A plain `true`
/// or `false` is a boolean; a plain `null` or `~`, or nothing, is null.
/// Blank lines and `#` comment lines are skipped. A duplicate key, a list
/// item, an indented continuation or a nested mapping is an error that says
/// what it found.
pub fn parse(block: &str) -> Result<Fields, ParseError> {
    let mut fields = Fields::default();
    for (index, raw) in block.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(ParseError {
                line: number,
                message: "an indented line; frontmatter here is one `key: value` per line, \
                          with no nesting or continuation"
                    .to_string(),
            });
        }
        if trimmed.starts_with("- ") || trimmed == "-" {
            return Err(ParseError {
                line: number,
                message: "a list item; frontmatter here is one `key: value` per line".to_string(),
            });
        }
        let Some((key, rest)) = line.split_once(':') else {
            return Err(ParseError {
                line: number,
                message: format!("`{trimmed}` is not `key: value`"),
            });
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ParseError {
                line: number,
                message: format!("`{key}` is not a key: letters, digits, `_` and `-`"),
            });
        }
        if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
            // `key:value` with no space is a plain scalar in YAML, and almost
            // always a mistake; `http://…` is the common victim.
            return Err(ParseError {
                line: number,
                message: format!("`{key}:` needs a space before its value"),
            });
        }
        if fields.get(key).is_some() {
            return Err(ParseError {
                line: number,
                message: format!("`{key}` is given twice"),
            });
        }
        let value = scalar(rest.trim(), number)?;
        fields.entries.push((key.to_string(), value));
    }
    Ok(fields)
}

/// One value, as written after the colon.
fn scalar(text: &str, line: usize) -> Result<Scalar, ParseError> {
    if text.is_empty() {
        return Ok(Scalar::Null);
    }
    if let Some(inner) = text.strip_prefix('"') {
        return double_quoted(inner, line).map(Scalar::Text);
    }
    if let Some(inner) = text.strip_prefix('\'') {
        return single_quoted(inner, line).map(Scalar::Text);
    }
    if text.starts_with(['[', '{', '|', '>', '&', '*', '!', '%', '@', '`']) {
        return Err(ParseError {
            line,
            message: format!(
                "`{text}` begins a value this frontmatter does not read (a list, a mapping, a \
                 block scalar or a tag); quote it to use it as text"
            ),
        });
    }
    // A plain scalar runs to a ` #` comment, as YAML's does.
    let plain = match text.find(" #") {
        Some(at) => text[..at].trim_end(),
        None => text,
    };
    Ok(match plain {
        "true" => Scalar::Bool(true),
        "false" => Scalar::Bool(false),
        "null" | "~" => Scalar::Null,
        other => Scalar::Text(other.to_string()),
    })
}

/// The text of a double-quoted value, `inner` starting after the opening
/// quote; anything after the closing quote may only be a comment.
fn double_quoted(inner: &str, line: usize) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return trailing(chars.as_str(), line).map(|()| out),
            '\\' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                    match code {
                        Some(code) => out.push(code),
                        None => {
                            return Err(ParseError {
                                line,
                                message: format!("`\\u{hex}` is not a character escape"),
                            });
                        }
                    }
                }
                Some(other) => {
                    return Err(ParseError {
                        line,
                        message: format!("`\\{other}` is not an escape this frontmatter reads"),
                    });
                }
                None => break,
            },
            other => out.push(other),
        }
    }
    Err(ParseError {
        line,
        message: "a double-quoted value with no closing quote".to_string(),
    })
}

/// The text of a single-quoted value, where `''` is one quote.
fn single_quoted(inner: &str, line: usize) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut rest = inner;
    loop {
        let Some(at) = rest.find('\'') else {
            return Err(ParseError {
                line,
                message: "a single-quoted value with no closing quote".to_string(),
            });
        };
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('\'') {
            out.push('\'');
            rest = after;
            continue;
        }
        return trailing(rest, line).map(|()| out);
    }
}

/// What may follow a closing quote: whitespace, then nothing or a comment.
fn trailing(rest: &str, line: usize) -> Result<(), ParseError> {
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err(ParseError {
            line,
            message: format!("`{rest}` after a closing quote"),
        })
    }
}

/// `value` as a double-quoted frontmatter scalar that [`parse`] reads back
/// exactly, whatever it holds — a colon, a quote, a newline, a `#`.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let scanned = scan("just prose\n").expect("scans");
        assert_eq!(scanned.frontmatter, None);
        assert_eq!(scanned.body, "just prose\n");
    }

    #[test]
    fn frontmatter_is_separated_from_the_body_whatever_the_line_endings() {
        let scanned = scan("---\nkey: value\n---\nbody\n").expect("scans");
        assert_eq!(scanned.frontmatter, Some("key: value\n"));
        assert_eq!(scanned.body, "body\n");

        let scanned = scan("---\r\nkey: value\r\n---\r\nbody\r\n").expect("scans");
        assert_eq!(scanned.frontmatter, Some("key: value\r\n"));
        assert_eq!(scanned.body, "body\r\n");

        let scanned = scan("\u{feff}---\nkey: value\n---\nbody\n").expect("scans");
        assert_eq!(scanned.frontmatter, Some("key: value\n"));
    }

    #[test]
    fn a_closing_delimiter_at_end_of_file_leaves_an_empty_body() {
        let scanned = scan("---\nkey: value\n---").expect("scans");
        assert_eq!(scanned.frontmatter, Some("key: value\n"));
        assert_eq!(scanned.body, "");
    }

    #[test]
    fn unterminated_frontmatter_is_an_error_and_empty_frontmatter_is_not() {
        assert_eq!(scan("---\nkey: value\nbody\n"), Err(Unterminated));
        let scanned = scan("---\n---\nbody\n").expect("scans");
        assert_eq!(scanned.frontmatter, Some(""));
    }

    #[test]
    fn plain_quoted_and_boolean_values_are_read() {
        let fields = parse(
            "name: reviewer\n\
             description: \"fix: the thing\\n(two lines)\"\n\
             note: 'it''s fine' # a comment\n\
             url: https://example.com/a#b\n\
             disable-model-invocation: true\n\
             # a comment line\n\
             \n\
             empty:\n\
             nothing: ~\n",
        )
        .expect("parses");

        assert_eq!(fields.text("name"), Some("reviewer"));
        assert_eq!(
            fields.text("description"),
            Some("fix: the thing\n(two lines)")
        );
        assert_eq!(fields.text("note"), Some("it's fine"));
        assert_eq!(fields.text("url"), Some("https://example.com/a#b"));
        assert_eq!(fields.boolean("disable-model-invocation"), Ok(Some(true)));
        assert_eq!(fields.get("empty"), Some(&Scalar::Null));
        assert_eq!(fields.get("nothing"), Some(&Scalar::Null));
        assert_eq!(fields.text("empty"), None);
        assert_eq!(fields.boolean("missing"), Ok(None));
        assert_eq!(
            fields.keys().collect::<Vec<_>>(),
            [
                "name",
                "description",
                "note",
                "url",
                "disable-model-invocation",
                "empty",
                "nothing"
            ]
        );
    }

    #[test]
    fn a_plain_value_stops_at_a_comment_and_keeps_an_inner_colon() {
        let fields = parse("description: remember: the parser is strict # really\n").unwrap();
        assert_eq!(
            fields.text("description"),
            Some("remember: the parser is strict")
        );
    }

    #[test]
    fn two_spellings_of_one_key_are_one_field() {
        let fields = parse("disable_model_invocation: \"true\"\n").unwrap();
        assert_eq!(
            fields.boolean_any(&["disable-model-invocation", "disable_model_invocation"]),
            Ok(Some(true))
        );
        assert_eq!(fields.text_any(&["argument-hint", "argument_hint"]), None);
    }

    #[test]
    fn what_is_not_flat_is_refused_with_the_reason() {
        for (block, expected) in [
            ("tags:\n  - a\n", "indented"),
            ("- a\n", "list item"),
            ("just words\n", "not `key: value`"),
            ("a key: value\n", "not a key"),
            ("url:https://x\n", "needs a space"),
            ("name: a\nname: b\n", "given twice"),
            ("items: [a, b]\n", "quote it"),
            ("text: |\n  block\n", "quote it"),
            ("name: \"open\n", "no closing quote"),
            ("name: 'open\n", "no closing quote"),
            ("name: \"a\" b\n", "after a closing quote"),
            ("name: \"\\q\"\n", "not an escape"),
        ] {
            let error = parse(block).expect_err(block);
            assert!(error.to_string().contains(expected), "{block:?}: {error}");
        }
        assert_eq!(
            parse("flag: maybe\n")
                .unwrap()
                .boolean("flag")
                .unwrap_err()
                .message,
            "`flag` must be true or false, not `maybe`"
        );
    }

    #[test]
    fn quote_round_trips_anything() {
        for value in [
            "plain",
            "remember: the parser is strict",
            "a \"quoted\" word and a 'single'",
            "back\\slash",
            "two\nlines\twith\ttabs",
            "# not a comment",
            "",
            "ends with a backslash \\",
            "\u{1}control",
        ] {
            let block = format!("key: {}\n", quote(value));
            assert_eq!(
                parse(&block).expect(&block).text("key"),
                Some(value),
                "{block}"
            );
        }
    }
}
