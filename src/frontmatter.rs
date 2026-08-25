//! YAML-ish frontmatter shared by the two kinds of markdown file Kamui loads: user commands
//! (`commands.rs`) and skills (`skills.rs`).
//!
//! Both had their own copy of this. The copies had already drifted: skills lower-cased keys, so
//! a file written `Description:` worked there and silently produced no description for a command
//! — for the same file, in the same tool. Only the set of keys each caller cares about differs,
//! so that is all each one still decides.

use std::collections::HashMap;

/// Splits `content` into its frontmatter keys and the body beneath.
///
/// Returns `None` when there is no frontmatter block at all, which is not an error: a bare
/// markdown file is a valid command and a valid skill. Keys are lower-cased, values are trimmed
/// and unquoted, blank and `#` comment lines are skipped, and empty values are dropped so a
/// caller never has to distinguish "absent" from "present but blank".
pub fn split(content: &str) -> Option<(HashMap<String, String>, &str)> {
    let normalized = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = normalized.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let (front, body) = split_at_closing_fence(rest)?;

    let mut keys = HashMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).trim().to_string();
        if value.is_empty() {
            continue;
        }
        keys.insert(key.trim().to_ascii_lowercase(), value);
    }
    Some((keys, body))
}

/// The frontmatter ends at the first line that is exactly `---`.
fn split_at_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_frontmatter_is_left_alone() {
        assert!(split("# Just a heading\n").is_none());
        // An opening fence with no closing one is not frontmatter either, so the body is not
        // swallowed by a stray `---`.
        assert!(split("---\ndescription: x\n").is_none());
    }

    #[test]
    fn keys_are_case_insensitive() {
        // The divergence this module exists to remove: skills accepted `Description:`, commands
        // did not, and produced no description at all rather than complaining.
        let (keys, body) = split("---\nDescription: hello\n---\nbody\n").expect("frontmatter");
        assert_eq!(keys.get("description").map(String::as_str), Some("hello"));
        assert_eq!(body, "body\n");
    }

    #[test]
    fn values_are_unquoted_and_blank_ones_dropped() {
        let (keys, _) =
            split("---\nname: \"quoted\"\ndescription:   \nother: 'single'\n---\n").expect("front");
        assert_eq!(keys.get("name").map(String::as_str), Some("quoted"));
        assert_eq!(keys.get("other").map(String::as_str), Some("single"));
        assert!(
            !keys.contains_key("description"),
            "blank is absent, not empty"
        );
    }

    #[test]
    fn comments_and_junk_lines_are_skipped() {
        let (keys, _) = split("---\n# a comment\nno colon here\nname: kept\n---\n").expect("front");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys.get("name").map(String::as_str), Some("kept"));
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_fence() {
        let (keys, _) = split("\u{feff}---\nname: kept\n---\n").expect("front");
        assert_eq!(keys.get("name").map(String::as_str), Some("kept"));
    }

    #[test]
    fn crlf_files_parse_the_same_as_lf_ones() {
        let (keys, body) = split("---\r\nname: kept\r\n---\r\nbody\r\n").expect("front");
        assert_eq!(keys.get("name").map(String::as_str), Some("kept"));
        assert_eq!(body, "body\r\n");
    }
}
