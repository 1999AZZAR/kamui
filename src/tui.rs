use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Helper};
use std::borrow::Cow::{self, Borrowed};

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
    ("exit", "Save and quit"),
    ("plan", "Enter Plan Mode"),
    ("skills", "List discovered skills"),
];

#[derive(Debug, Clone)]
struct Candidate {
    /// Without leading `/` (e.g. `help`, `my-skill`)
    name: String,
    /// `[skill]`, `[cmd]`, or `[builtin]`
    badge: &'static str,
    description: String,
}

/// Completer that merges built-ins + custom commands + skills.
/// Display shows `/{name}  {badge}  {description}`, replacement is `/{name} `.
pub struct SlashCompleter {
    candidates: Vec<Candidate>,
}

impl SlashCompleter {
    pub fn new(
        commands: &[crate::commands::CustomCommand],
        skills: &[crate::skills::Skill],
        disabled: &std::collections::HashSet<String>,
    ) -> Self {
        let mut candidates = Vec::new();
        for (name, desc) in BUILTINS {
            candidates.push(Candidate {
                name: name.to_string(),
                badge: "[builtin]",
                description: desc.to_string(),
            });
        }
        for cmd in commands {
            candidates.push(Candidate {
                name: cmd.name.clone(),
                badge: "[cmd]",
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
            candidates.push(Candidate {
                name: skill.name.clone(),
                badge: "[skill]",
                description: skill.description.clone(),
            });
        }
        // Sort by name for stable popup order (same as /commands and /skills listings).
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        Self { candidates }
    }

    /// For tests: build from explicit lists without touching filesystem.
    #[cfg(test)]
    pub fn from_parts(
        builtins: &[(&str, &str)],
        commands: Vec<(&str, &str)>,
        skills: Vec<(&str, &str)>,
    ) -> Self {
        let mut candidates = Vec::new();
        for (name, desc) in builtins {
            candidates.push(Candidate {
                name: name.to_string(),
                badge: "[builtin]",
                description: desc.to_string(),
            });
        }
        for (name, desc) in commands {
            candidates.push(Candidate {
                name: name.to_string(),
                badge: "[cmd]",
                description: desc.to_string(),
            });
        }
        for (name, desc) in skills {
            candidates.push(Candidate {
                name: name.to_string(),
                badge: "[skill]",
                description: desc.to_string(),
            });
        }
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        Self { candidates }
    }

    fn pairs_for(&self, prefix: &str) -> Vec<Pair> {
        // prefix includes leading `/`, e.g. "/" or "/he"
        let needle = prefix.trim_start_matches('/').to_ascii_lowercase();
        self.candidates
            .iter()
            .filter(|c| {
                if needle.is_empty() {
                    true
                } else {
                    c.name.to_ascii_lowercase().starts_with(&needle)
                }
            })
            .map(|c| Pair {
                display: format!("/{:<18} {:<9} {}", c.name, c.badge, c.description),
                replacement: format!("/{} ", c.name),
            })
            .collect()
    }
}

impl Completer for SlashCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let prefix = &line[..pos];
        let trimmed = prefix.trim_start();
        let offset = prefix.len() - trimmed.len();
        if !trimmed.starts_with('/') {
            return Ok((0, Vec::new()));
        }
        if trimmed.contains(' ') {
            return Ok((0, Vec::new()));
        }
        let pairs = self.pairs_for(trimmed);
        Ok((offset, pairs))
    }
}

impl Hinter for SlashCompleter {
    type Hint = String;
}

impl Highlighter for SlashCompleter {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        // No syntax highlighting in fenced code blocks (and no highlighting at all per Q5=A).
        Borrowed(line)
    }

    fn highlight_char(
        &self,
        line: &str,
        _pos: usize,
        _kind: rustyline::highlight::CmdKind,
    ) -> bool {
        let _ = line;
        false
    }
}

impl Validator for SlashCompleter {}

impl Helper for SlashCompleter {}

/// Whether the line editor + popup should be active.
/// Matches `Ui::stdio().interactive()` (TTY on both stdin/stdout + NO_COLOR not set).
pub fn is_interactive() -> bool {
    crate::terminal::Ui::stdio().interactive()
}

pub fn editor_config() -> Config {
    Config::builder()
        .completion_type(CompletionType::List)
        .completion_show_all_if_ambiguous(true)
        .max_history_size(1000)
        .unwrap()
        .auto_add_history(true)
        .history_ignore_dups(true)
        .unwrap()
        .build()
}

/// Read one line interactively with history + completer.
/// Returns `None` on EOF/Ctrl+C (caller should treat as shutdown).
/// Runs on the current thread (caller should `spawn_blocking` this).
pub fn read_line_blocking(
    commands: &[crate::commands::CustomCommand],
    skills: &[crate::skills::Skill],
    disabled: &std::collections::HashSet<String>,
    history: &[String],
) -> Option<String> {
    let config = editor_config();
    let helper = SlashCompleter::new(commands, skills, disabled);
    let mut editor = match rustyline::Editor::with_config(config) {
        Ok(e) => e,
        Err(_) => return None,
    };
    editor.set_helper(Some(helper));
    for entry in history.iter() {
        let _ = editor.add_history_entry(entry.as_str());
    }
    // Arrow keys navigate the popup when in "/" context, otherwise history.
    // Esc dismisses the popup (Abort). Tab / Shift-Tab also cycle completions.
    struct SlashNav;
    impl rustyline::ConditionalEventHandler for SlashNav {
        fn handle(
            &self,
            evt: &rustyline::Event,
            _n: rustyline::RepeatCount,
            _positive: bool,
            ctx: &rustyline::EventContext,
        ) -> Option<rustyline::Cmd> {
            let line = ctx.line();
            let pos = ctx.pos().min(line.len());
            let prefix = &line[..pos];
            let trimmed = prefix.trim_start();
            let in_slash = trimmed.starts_with('/') && !trimmed.contains(' ');
            if !in_slash {
                return None;
            }
            if let rustyline::Event::KeySeq(seq) = evt
                && seq.len() == 1
            {
                let k = &seq[0];
                if *k == rustyline::KeyEvent(rustyline::KeyCode::Up, rustyline::Modifiers::NONE) {
                    return Some(rustyline::Cmd::CompleteBackward);
                }
                if *k == rustyline::KeyEvent(rustyline::KeyCode::Down, rustyline::Modifiers::NONE) {
                    return Some(rustyline::Cmd::Complete);
                }
                if *k == rustyline::KeyEvent(rustyline::KeyCode::Esc, rustyline::Modifiers::NONE) {
                    return Some(rustyline::Cmd::Abort);
                }
            }
            None
        }
    }
    editor.bind_sequence(
        rustyline::Event::Any,
        rustyline::EventHandler::Conditional(Box::new(SlashNav)),
    );
    match editor.readline("> ") {
        Ok(line) => Some(line),
        Err(rustyline::error::ReadlineError::Interrupted) => None,
        Err(rustyline::error::ReadlineError::Eof) => None,
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completer() -> SlashCompleter {
        SlashCompleter::from_parts(
            &[("help", "Show help"), ("exit", "Quit")],
            vec![("review", "Review code")],
            vec![("my-skill", "Does stuff")],
        )
    }

    #[test]
    fn slash_prefix_returns_all() {
        let c = completer();
        let pairs = c.pairs_for("/");
        assert_eq!(pairs.len(), 4);
        // Sorted by name: exit, help, my-skill, review
        assert!(pairs.iter().any(|p| p.replacement == "/help "));
        assert!(pairs.iter().any(|p| p.replacement == "/my-skill "));
    }

    #[test]
    fn filtering_is_case_insensitive_and_prefix_only() {
        let c = completer();
        let pairs = c.pairs_for("/He");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].replacement, "/help ");
    }

    #[test]
    fn badges_are_correct() {
        let c = completer();
        let pairs = c.pairs_for("/");
        let help = pairs.iter().find(|p| p.replacement == "/help ").unwrap();
        assert!(help.display.contains("[builtin]"));
        let review = pairs.iter().find(|p| p.replacement == "/review ").unwrap();
        assert!(review.display.contains("[cmd]"));
        let skill = pairs
            .iter()
            .find(|p| p.replacement == "/my-skill ")
            .unwrap();
        assert!(skill.display.contains("[skill]"));
    }

    #[test]
    fn display_contains_description() {
        let c = completer();
        let pairs = c.pairs_for("/review");
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].display.contains("Review code"));
    }

    #[test]
    fn complete_returns_empty_when_not_slash() {
        let c = completer();
        let h: &'static rustyline::history::DefaultHistory =
            Box::leak(Box::new(rustyline::history::DefaultHistory::new()));
        let ctx = rustyline::Context::new(h);
        let (start, pairs) = c.complete("hello", 5, &ctx).unwrap();
        assert!(pairs.is_empty());
        assert_eq!(start, 0);
    }

    #[test]
    fn complete_returns_all_on_slash() {
        let c = completer();
        let h: &'static rustyline::history::DefaultHistory =
            Box::leak(Box::new(rustyline::history::DefaultHistory::new()));
        let ctx = rustyline::Context::new(h);
        let (start, pairs) = c.complete("/", 1, &ctx).unwrap();
        assert_eq!(start, 0);
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn complete_filters_on_prefix() {
        let c = completer();
        let h: &'static rustyline::history::DefaultHistory =
            Box::leak(Box::new(rustyline::history::DefaultHistory::new()));
        let ctx = rustyline::Context::new(h);
        let (_, pairs) = c.complete("/he", 3, &ctx).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].replacement, "/help ");
    }

    #[test]
    fn complete_ignores_after_space() {
        let c = completer();
        let h: &'static rustyline::history::DefaultHistory =
            Box::leak(Box::new(rustyline::history::DefaultHistory::new()));
        let ctx = rustyline::Context::new(h);
        let (_, pairs) = c.complete("/help extra", 11, &ctx).unwrap();
        assert!(pairs.is_empty());
    }

    #[test]
    fn highlight_is_passthrough() {
        let c = completer();
        let line = "```rust\nfn main() {}\n```";
        assert_eq!(c.highlight(line, 0), line);
    }
}
