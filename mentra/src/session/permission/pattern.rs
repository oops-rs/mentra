//! Wildcard matching for remembered permission rule patterns.
//!
//! A rule pattern is matched against the JSON encoding of a tool call's
//! structured input. That string is data, not a filesystem path, and the
//! distinction is not cosmetic: a path globber stops `*` at `/`, so the moment
//! a preview carries an absolute path — a `cwd`, a file argument — every key
//! `serde_json` writes after it becomes unreachable. The rule still saves, the
//! store still reports success, and the operator is told nothing; the rule
//! simply never answers a call again. Path globbers also read JSON's own
//! punctuation as syntax, taking `{`…`}` for alternation and `[`…`]` for a
//! character class, so a pattern quoting the front of an object matches
//! something other than the object it quotes.
//!
//! So the syntax here is deliberately small and has no separator at all:
//!
//! - `*` matches any run of characters, `/` included, and `**` means the same
//!   thing (patterns written when `**` was the only way to cross a separator
//!   keep working unchanged).
//! - `?` matches exactly one character — one `char`, not one byte, so a
//!   pattern stays predictable over non-ASCII input.
//! - Everything else, punctuation included, is literal.
//!
//! Matching is anchored: the pattern must account for the whole string, which
//! is why a substring rule is written `*needle*`.

use std::str::Chars;

/// Whether `pattern` matches the whole of `text`.
///
/// Greedy with backtracking: each `*` first takes as little as possible and is
/// widened one character at a time only when the rest of the pattern fails, so
/// a match is found whenever one exists.
pub(crate) fn matches(pattern: &str, text: &str) -> bool {
    let mut pattern_rest = pattern.chars();
    let mut text_rest = text.chars();
    // The pattern following the most recent `*`, paired with the text that
    // star has not yet swallowed. Backtracking is letting that star eat one
    // more character and retrying the rest of the pattern from there. Only the
    // last star needs remembering: once a prefix has matched, an earlier star
    // widening cannot rescue a failure a later one can.
    let mut widest_star: Option<(Chars<'_>, Chars<'_>)> = None;

    loop {
        let mut pattern_next = pattern_rest.clone();
        match pattern_next.next() {
            Some('*') => {
                pattern_rest = pattern_next;
                widest_star = Some((pattern_rest.clone(), text_rest.clone()));
                continue;
            }
            Some(expected) => {
                let mut text_next = text_rest.clone();
                if let Some(actual) = text_next.next()
                    && (expected == '?' || expected == actual)
                {
                    pattern_rest = pattern_next;
                    text_rest = text_next;
                    continue;
                }
            }
            None => {
                if text_rest.clone().next().is_none() {
                    return true;
                }
            }
        }

        // The pattern cannot account for what is at this position. Widen the
        // last star by one character, or admit there is nothing left to widen.
        let Some((star_pattern, star_text)) = widest_star.as_mut() else {
            return false;
        };
        if star_text.next().is_none() {
            return false;
        }
        pattern_rest = star_pattern.clone();
        text_rest = star_text.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    const PREVIEW: &str =
        r#"{"body":"cargo test","cwd":"/Users/dev/basis","mode":"command","target":"mac"}"#;

    #[test]
    fn a_star_crosses_a_path_separator() {
        assert!(matches(r#"*"target":"mac"*"#, PREVIEW));
        assert!(matches(r#"*"mode":"command"*"#, PREVIEW));
    }

    #[test]
    fn two_stars_mean_what_one_star_means() {
        assert!(matches(r#"**"target":"mac"**"#, PREVIEW));
        assert!(matches("**", PREVIEW));
        assert!(matches("***", PREVIEW));
    }

    #[test]
    fn json_punctuation_is_literal() {
        assert!(matches(r#"{"body":"cargo test"*"#, PREVIEW));
        assert!(matches(r#"*"cwd":"/Users/dev/basis"*"#, PREVIEW));
        // Brace alternation and character classes are not syntax here, so a
        // pattern naming them matches only text that contains them.
        assert!(!matches("{a,b}", "a"));
        assert!(matches("{a,b}", "{a,b}"));
        assert!(!matches("[abc]", "a"));
        assert!(matches("[abc]", "[abc]"));
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        assert!(matches("a?c", "abc"));
        assert!(matches("a?c", "a/c"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
    }

    #[test]
    fn a_question_mark_counts_characters_not_bytes() {
        // One multi-byte char is one `?`, so a pattern behaves the same over
        // text a host did not write in ASCII.
        assert!(matches("a?c", "aéc"));
        assert!(matches("?", "é"));
        assert!(!matches("??", "é"));
    }

    #[test]
    fn matching_is_anchored() {
        assert!(matches("cargo test", "cargo test"));
        assert!(!matches("cargo", "cargo test"));
        assert!(!matches("test", "cargo test"));
        assert!(matches("cargo*", "cargo test"));
        assert!(matches("*test", "cargo test"));
    }

    #[test]
    fn empty_pattern_matches_only_empty_text() {
        assert!(matches("", ""));
        assert!(!matches("", "a"));
        assert!(matches("*", ""));
    }

    #[test]
    fn a_star_backtracks_until_the_rest_of_the_pattern_fits() {
        // The first candidate for each star is wrong here; only widening in
        // turn finds the match.
        assert!(matches("*a*b", "xaybzb"));
        assert!(matches("*ab*cd*", "zzabzzcdzz"));
        assert!(!matches("*a*b", "xaybz"));
    }

    #[test]
    fn a_pattern_that_names_something_absent_does_not_match() {
        assert!(!matches(r#"**"target":"linux"**"#, PREVIEW));
        assert!(!matches(r#"**"mode":"shell"**"#, PREVIEW));
    }
}
