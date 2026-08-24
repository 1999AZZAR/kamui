use crate::terminal::{Style, Ui};
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
    ("expand", "Toggle full or collapsed tool output (Ctrl+O)"),
];

#[derive(Debug, Clone)]
struct Candidate {
    /// Without leading `/` (e.g. `help`, `my-skill`)
    name: String,
    description: String,
}

/// Completer that merges built-ins + custom commands + skills.
/// Display shows `{name}  {description}`, replacement is `/{name} `.
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
                description: desc.to_string(),
            });
        }
        for cmd in commands {
            candidates.push(Candidate {
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
            candidates.push(Candidate {
                name: skill.name.clone(),
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
                description: desc.to_string(),
            });
        }
        for (name, desc) in commands {
            candidates.push(Candidate {
                name: name.to_string(),
                description: desc.to_string(),
            });
        }
        for (name, desc) in skills {
            candidates.push(Candidate {
                name: name.to_string(),
                description: desc.to_string(),
            });
        }
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        Self { candidates }
    }

    fn pairs_for(&self, prefix: &str) -> Vec<Pair> {
        // prefix includes leading `/`, e.g. "/" or "/he"
        let needle = prefix.trim_start_matches('/').to_ascii_lowercase();
        let filtered: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|c| {
                if needle.is_empty() {
                    true
                } else {
                    c.name.to_ascii_lowercase().starts_with(&needle)
                }
            })
            .collect();
        filtered
            .into_iter()
            .map(|c| {
                // ponytail: truncate — long skill descs (300+ chars) wrap & break last_lines counting
                let d = if c.description.len() > 60 {
                    format!("{}…", &c.description[..59])
                } else {
                    c.description.clone()
                };
                Pair {
                    display: format!("{:<20}  {}", c.name, d),
                    replacement: format!("/{} ", c.name),
                }
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

fn slash_candidates(
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

fn filter_candidates<'a>(candidates: &'a [Candidate], needle: &str) -> Vec<&'a Candidate> {
    if needle.is_empty() {
        return candidates.iter().collect();
    }
    let n = needle.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|c| c.name.to_ascii_lowercase().starts_with(&n))
        .collect()
}

fn read_line_with_slash_popup(candidates: &[Candidate], history: &[String]) -> Option<String> {
    use dialoguer::console::{Key, Term};
    let term = Term::stdout();
    if !term.is_term() {
        return None;
    }
    let _ = term.hide_cursor();
    let prompt = Ui::stdio().style("> ", &[Style::Cyan, Style::Bold]);
    let mut buf = String::new();
    let mut history_idx = history.len();
    let mut saved_buf = String::new();
    let mut selected: usize = 0;
    let mut last_lines: usize = 0;

    let render = |buf: &str, selected: usize, last_lines: &mut usize, term: &Term| {
        let is_slash = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let mut out = String::new();
        let styled_buf = Ui::stdio().style(buf, &[Style::White]);
        out.push_str(&format!("\r\x1b[J{prompt}{styled_buf}"));
        if is_slash {
            let needle = buf
                .trim_start()
                .trim_start_matches('/')
                .to_ascii_lowercase();
            let filtered = filter_candidates(candidates, &needle);
            let total = filtered.len();
            let visible = 10usize;
            let start = if total <= visible {
                0
            } else {
                let s = selected.saturating_sub(visible / 2);
                s.min(total - visible)
            };
            out.push_str("\n\x1b[2m─\x1b[0m");
            let term_w = term.size().1 as usize;
            let max_desc = term_w.saturating_sub(26).clamp(20, 60);
            for (i, c) in filtered.iter().skip(start).take(visible).enumerate() {
                let idx = start + i;
                let is_on = idx == selected;
                let prefix = if is_on { "→ " } else { "  " };
                let hl_on = if is_on { "\x1b[7m" } else { "" };
                let desc = if c.description.len() > max_desc {
                    format!("{}…", &c.description[..max_desc.saturating_sub(1)])
                } else {
                    c.description.clone()
                };
                out.push_str(&format!("\n{hl_on}{prefix}{:<20}  {}\x1b[0m", c.name, desc));
            }
            if total > 0 {
                out.push_str(&format!("\n\x1b[2m  ({}/{}) \x1b[0m", selected + 1, total));
            } else {
                out.push_str("\n\x1b[2m  (no match)\x1b[0m");
            }
        }
        let rows_below = out.matches('\n').count();
        if *last_lines > 0 {
            let _ = term.write_str(&format!("\x1b[{}A\x1b[J", last_lines));
        }
        let _ = term.write_str(&out);
        let _ = term.flush();
        *last_lines = rows_below;
    };

    render(&buf, selected, &mut last_lines, &term);
    loop {
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => {
                let _ = term.show_cursor();
                return None;
            }
        };
        let is_slash = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let needle = buf
            .trim_start()
            .trim_start_matches('/')
            .to_ascii_lowercase();
        let filtered = filter_candidates(candidates, &needle);
        match key {
            Key::ArrowUp => {
                if is_slash && !filtered.is_empty() {
                    if selected > 0 {
                        selected -= 1;
                    } else {
                        selected = filtered.len() - 1;
                    }
                } else if history_idx > 0 {
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                    history_idx -= 1;
                    buf = history[history_idx].clone();
                    selected = 0;
                }
                render(&buf, selected, &mut last_lines, &term);
            }
            Key::ArrowDown => {
                if is_slash && !filtered.is_empty() {
                    selected = (selected + 1) % filtered.len();
                } else if history_idx < history.len() {
                    history_idx += 1;
                    if history_idx == history.len() {
                        buf = saved_buf.clone();
                    } else {
                        buf = history[history_idx].clone();
                    }
                    selected = 0;
                }
                render(&buf, selected, &mut last_lines, &term);
            }
            Key::Enter => {
                if is_slash && !filtered.is_empty() {
                    let chosen = filtered[selected].name.clone();
                    buf = format!("/{chosen} ");
                }
                let _ = term.show_cursor();
                if last_lines > 0 {
                    let _ = term.write_str(&format!("\x1b[{}A\r\x1b[J", last_lines));
                } else {
                    let _ = term.write_str("\r\x1b[J");
                }
                let _ = term.write_str(&crate::render::render_user_prompt(&buf, Ui::stdio()));
                let _ = term.flush();
                return Some(buf);
            }
            Key::Escape => {
                if is_slash {
                    buf.clear();
                    selected = 0;
                    render(&buf, selected, &mut last_lines, &term);
                } else {
                    let _ = term.show_cursor();
                    if last_lines > 0 {
                        let _ = term.write_str(&format!("\x1b[{}A\r\x1b[J", last_lines));
                    } else {
                        let _ = term.write_str("\r\x1b[J");
                    }
                    let _ = term.flush();
                    return None;
                }
            }
            Key::Backspace => {
                if !buf.is_empty() {
                    buf.pop();
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                    render(&buf, selected, &mut last_lines, &term);
                }
            }
            Key::Char(c) => {
                if c == '\x03' {
                    let _ = term.show_cursor();
                    if last_lines > 0 {
                        let _ = term.write_str(&format!("\x1b[{}A\r\x1b[J", last_lines));
                    } else {
                        let _ = term.write_str("\r\x1b[J");
                    }
                    let _ = term.flush();
                    return None;
                }
                if c == '\x0f' {
                    // Ctrl+O: toggle tool output expand/collapse
                    let _ = term.show_cursor();
                    if last_lines > 0 {
                        let _ = term.write_str(&format!("\x1b[{}A\r\x1b[J", last_lines));
                    } else {
                        let _ = term.write_str("\r\x1b[J");
                    }
                    let _ = term.flush();
                    return Some("/expand".to_string());
                }
                buf.push(c);
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
                render(&buf, selected, &mut last_lines, &term);
            }
            _ => {}
        }
    }
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
    let candidates = slash_candidates(commands, skills, disabled);
    // The slash-popup owns the terminal. When stdout is a TTY, `None` from it means the
    // user quit (Ctrl+C/Escape/EOF) — do NOT fall through to rustyline, which would print
    // a second prompt and discard the typed line. rustyline is only for piped (non-TTY) input.
    if dialoguer::console::Term::stdout().is_term() {
        return read_line_with_slash_popup(&candidates, history);
    }
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
    fn display_shows_name_and_description() {
        let c = completer();
        let pairs = c.pairs_for("/");
        let help = pairs.iter().find(|p| p.replacement == "/help ").unwrap();
        assert!(help.display.contains("help"));
        assert!(help.display.contains("Show help"));
        let review = pairs.iter().find(|p| p.replacement == "/review ").unwrap();
        assert!(review.display.contains("review"));
        assert!(review.display.contains("Review code"));
        let skill = pairs
            .iter()
            .find(|p| p.replacement == "/my-skill ")
            .unwrap();
        assert!(skill.display.contains("my-skill"));
        assert!(skill.display.contains("Does stuff"));
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
