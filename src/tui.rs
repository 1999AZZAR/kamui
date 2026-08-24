/// Built-in slash commands with their short descriptions (kept in sync with `chat::print_help`).
pub const BUILTINS: &[(&str, &str)] = &[
    ("help", "Show available commands"),
    ("new", "Start a new session"),
    ("sessions", "List saved sessions"),
    ("resume", "Resume a session"),
    ("model", "List or switch provider profiles"),
    ("rename", "Rename a session"),
    ("search", "Search saved messages"),
    ("compact", "Summarize older messages"),
    ("undo", "Revert the last turn's file edits"),
    ("jobs", "List session and scheduled jobs"),
    ("index", "Rebuild the semantic-search index"),
    ("commands", "List your own prompt commands"),
    ("delete", "Delete a session"),
    ("stats", "Show current session usage"),
    ("usage", "Show token usage by day and month"),
    ("status", "Show project and connection status"),
    ("memory", "List remembered facts"),
    ("forget", "Forget a remembered fact"),
    ("expand", "Expand the last transcript card"),
    ("collapse", "Collapse the last transcript card"),
    ("exit", "Save and quit"),
    ("plan", "Enter Plan Mode"),
    ("skills", "List discovered skills"),
];

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    /// Without leading `/` (e.g. `help`, `my-skill`)
    pub(crate) name: String,
    pub(crate) description: String,
}

pub fn is_interactive() -> bool {
    let ui = crate::terminal::Ui::stdio();
    ui.interactive() && std::env::var_os("NO_COLOR").is_none()
}

pub(crate) fn slash_candidates(
    commands: &[crate::commands::CustomCommand],
    skills: &[crate::skills::Skill],
    disabled: &std::collections::HashSet<String>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (name, desc) in BUILTINS {
        out.push(Candidate {
            name: name.to_string(),
            description: desc.to_string(),
        });
    }
    for cmd in commands {
        out.push(Candidate {
            name: cmd.name.clone(),
            description: cmd
                .description
                .clone()
                .unwrap_or_else(|| format!("custom command ({})", cmd.source.label())),
        });
    }
    for skill in skills {
        if disabled.contains(&skill.name) {
            continue;
        }
        out.push(Candidate {
            name: skill.name.clone(),
            description: skill.description.clone(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Cut to at most `max` characters (never bytes), appending an ellipsis when trimmed. Byte
/// slicing here would panic mid-UTF-8 for descriptions with non-ASCII text.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}\u{2026}")
}

pub(crate) fn filter_candidates<'a>(
    candidates: &'a [Candidate],
    needle: &str,
) -> Vec<&'a Candidate> {
    if needle.is_empty() {
        return candidates.iter().collect();
    }
    let n = needle.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|c| c.name.to_ascii_lowercase().starts_with(&n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<Candidate> {
        let mut out: Vec<Candidate> = [
            ("help", "Show help"),
            ("exit", "Quit"),
            ("review", "Review code"),
            ("my-skill", "Does stuff"),
        ]
        .iter()
        .map(|(n, d)| Candidate {
            name: n.to_string(),
            description: d.to_string(),
        })
        .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    #[test]
    fn slash_prefix_returns_all() {
        let all = candidates();
        assert_eq!(filter_candidates(&all, "").len(), 4);
        // Sorted by name: exit, help, my-skill, review
        assert_eq!(all[0].name, "exit");
        assert_eq!(all[3].name, "review");
    }

    #[test]
    fn filtering_is_case_insensitive_and_prefix_only() {
        let all = candidates();
        let hits = filter_candidates(&all, "he");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "help");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(
            truncate_chars("\u{754c}\u{754c}\u{754c}", 2)
                .chars()
                .count(),
            2
        );
        assert_eq!(truncate_chars("abc", 5), "abc");
    }
}
