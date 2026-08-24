//! Borderless, full-bleed execution-log renderers for the interactive chat feed.
//!
//! Inspired by Pi/modern CLI agent design:
//! - No top/bottom ASCII border lines (`┌───`, `└───`, `────`).
//! - Full-bleed rectangular background blocks that fill the terminal width (`width`),
//!   avoiding jagged/abrupt colored text ends.
//! - Clear typographic hierarchy and refined color tones for dark terminals.
//! - Color is gated through `Ui::style`, so `NO_COLOR` and non-TTY output fall back to plain text.
//!
//! Wired into the chat loop in `chat.rs`; `render_assistant`/`render_final` stay plain because
//! streamed answers already go through the markdown renderer.
#![allow(dead_code)] // render_assistant / render_final await a plain-text feed

use dialoguer::console::Term;

use crate::terminal::{Style, Ui};

/// Minimum block width; terminals report 0 columns when not attached.
const MIN_WIDTH: usize = 40;

fn terminal_width() -> usize {
    let w = Term::stdout().size().1 as usize;
    if w == 0 { 80 } else { w }
}

/// Truncate `line` to at most `max` characters (by char, not byte).
fn truncate(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    line.chars().take(max).collect()
}

/// Split `text` into body rows (trailing whitespace trimmed per line).
fn body(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Format a single line padded with spaces to `width` columns for full-bleed background fill.
///
/// If `line` with `prefix` is shorter than `width`, spaces are appended so that `ui.style`
/// colors the entire terminal row from margin to margin without any ragged right edge.
fn pad_row(prefix: &str, text: &str, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let prefix_chars = prefix.chars().count();
    let max_text = width.saturating_sub(prefix_chars);
    let truncated = truncate(text, max_text);
    let current_chars = prefix_chars + truncated.chars().count();
    let fill = width.saturating_sub(current_chars);
    let mut out = String::with_capacity(prefix.len() + truncated.len() + fill);
    out.push_str(prefix);
    out.push_str(&truncated);
    for _ in 0..fill {
        out.push(' ');
    }
    out
}

/// Render multiple lines with full-bleed background and no top/bottom border lines.
fn bleed_block(
    header: Option<(&str, &str, &[Style])>, // (prefix, title, styles)
    body_prefix: &str,
    body_lines: &[String],
    width: usize,
    ui: Ui,
    body_styles: &[Style],
) -> String {
    let width = width.max(MIN_WIDTH);
    let mut out = String::new();

    if let Some((h_prefix, h_title, h_styles)) = header {
        let row = pad_row(h_prefix, h_title, width);
        out.push_str(&ui.style(&row, h_styles));
        out.push('\n');
    }

    for line in body_lines {
        let row = pad_row(body_prefix, line, width);
        out.push_str(&ui.style(&row, body_styles));
        out.push('\n');
    }

    out
}

/// Full-bleed user prompt on a blue background (no top/bottom border):
/// `  > <first line>`
/// `    <subsequent lines>`
pub fn render_user_prompt(text: &str, ui: Ui) -> String {
    let lines = body(text);
    if lines.is_empty() {
        return String::new();
    }
    let width = terminal_width();
    let mut out = String::new();

    for (i, line) in lines.iter().enumerate() {
        let prefix = if i == 0 { "  > " } else { "    " };
        let row = pad_row(prefix, line, width);
        out.push_str(&ui.style(&row, &[Style::BgBlue, Style::White, Style::Bold]));
        out.push('\n');
    }
    out
}

/// Full-bleed tool invocation on a subtle gray background (no top/bottom border):
/// `  Tool: <name>`
/// `    <args>`
pub fn render_tool_call(name: &str, args: &str, ui: Ui) -> String {
    let width = terminal_width();
    let body_lines = body(args);
    bleed_block(
        Some((
            "  Tool: ",
            name,
            &[Style::BgGray, Style::White, Style::Bold],
        )),
        "    ",
        &body_lines,
        width,
        ui,
        &[Style::BgGray, Style::White],
    )
}

/// Full-bleed tool output on a dark subtle background (no top/bottom border):
/// `  Tool Output`
/// `    <output line>`
pub fn render_tool_output(text: &str, ui: Ui) -> String {
    let width = terminal_width();
    let body_lines = body(text);
    bleed_block(
        Some((
            "  Tool Output",
            "",
            &[Style::BgDark, Style::White, Style::Bold],
        )),
        "    ",
        &body_lines,
        width,
        ui,
        &[Style::BgDark, Style::White],
    )
}

/// Subtle live status line: `⠋ <message>` in dim, no trailing newline.
pub fn render_progress(message: &str, ui: Ui) -> String {
    ui.style(&format!("⠋ {message}"), &[Style::Dim])
}

/// Dim system notice: `  • <message>` (clean text, no dash lines).
pub fn render_system(message: &str, ui: Ui) -> String {
    format!("{}\n", ui.style(&format!("  • {message}"), &[Style::Dim]))
}

/// Yellow warning notice: `  ⚠ <message>` (clean text, no dash lines).
pub fn render_warning(message: &str, ui: Ui) -> String {
    format!(
        "{}\n",
        ui.style(&format!("  ⚠ {message}"), &[Style::Yellow])
    )
}

/// Full-bleed error block on a red background (no top/bottom border):
/// `  Error: <first line>`
/// `    <subsequent lines>`
pub fn render_error(message: &str, ui: Ui) -> String {
    let lines = body(message);
    if lines.is_empty() {
        return String::new();
    }
    let width = terminal_width();
    let mut out = String::new();

    for (i, line) in lines.iter().enumerate() {
        let prefix = if i == 0 { "  Error: " } else { "    " };
        let row = pad_row(prefix, line, width);
        out.push_str(&ui.style(&row, &[Style::BgRed, Style::White, Style::Bold]));
        out.push('\n');
    }
    out
}

/// Full-bleed tool outcome on a green (success) or red (failure) background:
/// `  ✓ completed · 1ms · 218 chars`
pub fn render_tool_outcome(output: &str, elapsed: std::time::Duration, ui: Ui) -> String {
    let width = terminal_width();
    let duration_str = if elapsed.as_secs() > 0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}ms", elapsed.as_millis())
    };
    let mut out = String::new();

    match output.strip_prefix("Error: ") {
        Some(error) => {
            let text = format!("✗ failed · {duration_str} · {error}");
            let row = pad_row("  ", &text, width);
            out.push_str(&ui.style(&row, &[Style::BgRed, Style::White, Style::Bold]));
        }
        None => {
            let char_count = output.chars().count();
            let text = format!("✓ completed · {duration_str} · {char_count} chars");
            let row = pad_row("  ", &text, width);
            out.push_str(&ui.style(&row, &[Style::BgGreen, Style::White, Style::Bold]));
        }
    }
    out.push('\n');
    out
}

/// Full-bleed token usage and timing statistics banner:
/// `  Tokens: 12015 input + 123 output = 12138 total | TTFT: 2.6s | Time: 2.7s | Finish: stop`
pub fn render_usage_stats(stats_text: &str, ui: Ui) -> String {
    let width = terminal_width();
    let row = pad_row("  ", stats_text, width);
    format!("{}\n", ui.style(&row, &[Style::BgDark, Style::White]))
}

/// Plain assistant text (no box, no colour).
pub fn render_assistant(text: &str) -> String {
    format!("{text}\n")
}

/// Plain final/status line.
pub fn render_final(text: &str) -> String {
    format!("{text}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui() -> Ui {
        Ui::plain()
    }

    #[test]
    fn tool_call_renders_full_bleed_without_borders() {
        let rendered = render_tool_call("read_file", "short", ui());
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  Tool: read_file"));
        assert!(lines[1].starts_with("    short"));
        // No unicode border characters
        assert!(!rendered.contains('┌'));
        assert!(!rendered.contains('│'));
        assert!(!rendered.contains('└'));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn tool_outcome_renders_full_bleed_green_or_red() {
        let rendered = render_tool_outcome("hello", std::time::Duration::from_millis(5), ui());
        assert!(rendered.starts_with("  ✓ completed · 5ms · 5 chars"));
        assert!(rendered.ends_with('\n'));

        let rendered_err =
            render_tool_outcome("Error: nope", std::time::Duration::from_millis(5), ui());
        assert!(rendered_err.starts_with("  ✗ failed · 5ms · nope"));
        assert!(rendered_err.ends_with('\n'));
    }

    #[test]
    fn usage_stats_renders_full_bleed() {
        let rendered = render_usage_stats("Tokens: 100 total", ui());
        assert!(rendered.starts_with("  Tokens: 100 total"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn long_body_lines_are_truncated_and_padded_to_width() {
        let row = pad_row("  > ", &"x".repeat(200), 40);
        assert_eq!(row.chars().count(), 40);
        assert!(row.starts_with("  > xxxxx"));
    }

    #[test]
    fn short_body_lines_are_padded_to_width() {
        let row = pad_row("    ", "abc", 40);
        assert_eq!(row.chars().count(), 40);
        assert_eq!(&row[..7], "    abc");
        assert_eq!(&row[7..], " ".repeat(33));
    }

    #[test]
    fn user_prompt_renders_all_lines_full_bleed() {
        let rendered = render_user_prompt("line1\nline2", ui());
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("  > line1"));
        assert!(lines[1].starts_with("    line2"));
    }

    #[test]
    fn warning_renders_clean_notice_without_dashes() {
        let rendered = render_warning("stale fixtures", ui());
        assert_eq!(rendered, "  ⚠ stale fixtures\n");
    }

    #[test]
    fn system_renders_clean_notice_without_dashes() {
        let rendered = render_system("New chat", ui());
        assert_eq!(rendered, "  • New chat\n");
    }

    #[test]
    fn progress_is_single_line_without_newline() {
        let rendered = render_progress("searching…", ui());
        assert_eq!(rendered, "⠋ searching…");
    }

    #[test]
    fn assistant_and_final_are_plain_with_newline() {
        assert_eq!(render_assistant("hi"), "hi\n");
        assert_eq!(render_final("done"), "done\n");
    }
}
