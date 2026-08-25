//! Boxed, width-aware execution-log renderers for the interactive chat feed.
//!
//! Each `render_*` function returns a complete string (trailing newline except
//! `render_progress`, which is a live status line). Colour is gated through
//! `Ui::style`, so `NO_COLOR` and non-TTY output fall back to plain text.
//! Wired into the chat loop in `chat.rs`; `render_assistant`/`render_final` stay plain because
//! streamed answers already go through the markdown renderer.
#![allow(dead_code)] // render_assistant / render_final await a plain-text feed

use dialoguer::console::Term;

use crate::terminal::{Style, Ui};
use unicode_width::UnicodeWidthStr;

/// Minimum box width; terminals report 0 columns when not attached.
const MIN_WIDTH: usize = 40;

fn terminal_width() -> usize {
    Term::stdout().size().1 as usize
}

/// Truncate `line` to at most `max` characters (by char, not byte).
/// Split `text` into body rows (trailing whitespace trimmed per line).
fn body(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Draw a unicode box with a titled top border and `body_lines` inside.
///
/// Column `width` layout: `┌─ <title> ──…──┐`, one `│ <line> │` row per body
/// line (each truncated to `width - 4`), then `└──…──┘`. The top border uses
/// `title_style`; everything else uses `box_style`. Returns a trailing newline.
fn box_lines(
    title: &str,
    body_lines: &[String],
    width: usize,
    ui: Ui,
    title_style: &[Style],
    box_style: &[Style],
) -> String {
    let width = width.max(MIN_WIDTH);
    // Room between the bar that opens a row and the bar that closes it.
    let inner = width.saturating_sub(4);
    let title = crate::tui::truncate_chars(title, inner);
    let mut out = String::new();

    let lead = format!("\u{250c}\u{2500} {title} \u{2500}");
    let lead_width = UnicodeWidthStr::width(lead.as_str());
    let top = format!(
        "{lead}{}\u{2510}",
        "\u{2500}".repeat(width.saturating_sub(lead_width + 1))
    );
    out.push_str(&ui.style(&top, title_style));
    out.push('\n');

    for line in body_lines {
        // Padded to the same inner width as every other row. Without this the closing bar
        // landed wherever the text happened to end, so the box had no right edge at all.
        let text = crate::tui::truncate_chars(line, inner);
        let padding = inner.saturating_sub(UnicodeWidthStr::width(text.as_str()));
        let row = format!("\u{2502} {text}{} \u{2502}", " ".repeat(padding));
        out.push_str(&ui.style(&row, box_style));
        out.push('\n');
    }

    // `width - 2`, not `width - 1`: the two corners take a column each, so the old fill made
    // the bottom border one character wider than the top.
    let bottom = format!(
        "\u{2514}{}\u{2518}",
        "\u{2500}".repeat(width.saturating_sub(2))
    );
    out.push_str(&ui.style(&bottom, box_style));
    out.push('\n');
    out
}

/// `─── <label> ──…──` separator filling the terminal width.
fn separator(label: &str, width: usize) -> String {
    let width = width.max(MIN_WIDTH);
    let lead = format!("─── {label} ");
    let fill = "─".repeat(width.saturating_sub(lead.chars().count()));
    format!("{lead}{fill}")
}

/// Boxed user prompt on a blue background: `┌─ User ─…┐`.
pub fn render_user_prompt(text: &str, ui: Ui) -> String {
    box_lines(
        "User",
        &body(text),
        terminal_width(),
        ui,
        &[Style::BgBlue, Style::White, Style::Bold],
        &[Style::BgBlue],
    )
}

/// Boxed tool invocation: `┌─ Tool: <name> ─…┐` on a gray background.
pub fn render_tool_call(name: &str, args: &str, ui: Ui) -> String {
    box_lines(
        &format!("Tool: {name}"),
        &body(args),
        terminal_width(),
        ui,
        &[Style::BgGray, Style::White, Style::Bold],
        &[Style::BgGray],
    )
}

/// Boxed tool result: `┌─ Tool Output ─…┐`, darker bg than the call box (per prototype).
pub fn render_tool_output(text: &str, ui: Ui) -> String {
    box_lines(
        "Tool Output",
        &body(text),
        terminal_width(),
        ui,
        &[Style::BgDark, Style::White, Style::Bold],
        &[Style::BgDark],
    )
}

/// Subtle live status line: `⠋ <message>` in dim, no trailing newline.
pub fn render_progress(message: &str, ui: Ui) -> String {
    ui.style(&format!("⠋ {message}"), &[Style::Dim])
}

/// Dim separator for system notices: `─── <message> ──…──`.
pub fn render_system(message: &str, ui: Ui) -> String {
    format!(
        "{}\n",
        ui.style(&separator(message, terminal_width()), &[Style::Dim])
    )
}

/// Warning separator: `─── ⚠ <message> ──…──` in yellow.
pub fn render_warning(message: &str, ui: Ui) -> String {
    format!(
        "{}\n",
        ui.style(
            &separator(&format!("⚠ {message}"), terminal_width()),
            &[Style::Yellow]
        )
    )
}

/// Boxed error on a red background: `┌─ Error ─…┐`.
pub fn render_error(message: &str, ui: Ui) -> String {
    box_lines(
        "Error",
        &body(message),
        terminal_width(),
        ui,
        &[Style::BgRed, Style::White, Style::Bold],
        &[Style::BgRed],
    )
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
    fn box_has_titled_top_and_trailing_newline() {
        let rendered = box_lines(
            "Tool: read_file",
            &["short".to_string()],
            40,
            ui(),
            &[Style::Bold],
            &[],
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].starts_with("┌─ Tool: read_file ─"));
        assert_eq!(lines[0].chars().count(), 40);
        // Every row is the same width, so the box actually has a right edge. The bottom
        // border used to be one column wider than the top.
        for line in &lines {
            assert_eq!(line.chars().count(), 40, "ragged row: {line:?}");
        }
        assert!(lines[1].starts_with("│ short"));
        assert!(lines[1].ends_with("│"));
        assert!(lines[2].starts_with('└'));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn long_body_lines_are_truncated_to_width_minus_four() {
        let rendered = box_lines(
            "User",
            &["x".repeat(200)],
            40,
            ui(),
            &[Style::BgBlue],
            &[Style::BgBlue],
        );
        let row = rendered.lines().nth(1).unwrap();
        assert_eq!(row.chars().count(), 40);
        assert!(row.ends_with(" │"));
    }

    #[test]
    fn multi_line_body_renders_one_row_per_line() {
        let rendered = box_lines(
            "Output",
            &["a".to_string(), "b".to_string()],
            40,
            ui(),
            &[Style::Bold],
            &[],
        );
        assert_eq!(rendered.lines().count(), 4); // top + 2 rows + bottom
    }

    #[test]
    fn warning_separator_includes_icon_and_fills_width() {
        let rendered = render_warning("stale fixtures", ui());
        assert!(rendered.starts_with("─── ⚠ stale fixtures ─"));
        assert!(rendered.ends_with('\n'));
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
