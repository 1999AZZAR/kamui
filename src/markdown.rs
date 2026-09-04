//! Minimal markdown-to-ANSI rendering for streamed model output.
//!
//! Rendering happens a line at a time: deltas accumulate in a buffer and a line is only rendered
//! once its newline arrives, so output still appears as it streams while each line can be styled
//! as a whole. Almost every construct worth rendering (headings, fences, bullets) is line-scoped
//! anyway, and the ones that are not (bold, code spans) practically never straddle a line break.
//!
//! The supported subset is deliberately narrow, because this is a coding agent and mangling code
//! is worse than under-styling prose. In particular `_italic_` is **not** supported: it would
//! corrupt `snake_case` identifiers, which appear constantly in this tool's output. Single-`*`
//! italics are skipped for the same reason (`src/*.rs`). Only unambiguous constructs are styled.

use crate::terminal::{BOLD, CYAN, DIM, RESET};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use std::io::IsTerminal;

/// Line-buffered renderer for one streamed response. Holds the partial line and whether the
/// stream is currently inside a fenced code block.
pub struct Renderer {
    enabled: bool,
    line: String,
    in_fence: bool,
}

impl Renderer {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            line: String::new(),
            in_fence: false,
        }
    }

    /// A renderer that styles output only when stdout is a terminal, so piping or redirecting
    /// still produces clean text. Honours the `NO_COLOR` convention (<https://no-color.org>).
    pub fn for_stdout() -> Self {
        Self::new(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }

    /// Feed one streamed delta, returning the text to print now: every line completed by this
    /// delta, rendered. A trailing partial line stays buffered until its newline or `finish`.
    pub fn push(&mut self, delta: &str) -> String {
        self.line.push_str(delta);
        let mut out = String::new();
        while let Some(end) = self.line.find('\n') {
            let line: String = self.line.drain(..=end).collect();
            out.push_str(&self.render_line(line.trim_end_matches(['\n', '\r'])));
            out.push('\n');
        }
        out
    }

    /// Flush whatever is still buffered, for the end of a stream (or an interrupted one).
    pub fn finish(&mut self) -> String {
        if self.line.is_empty() {
            return String::new();
        }
        let line = std::mem::take(&mut self.line);
        self.render_line(line.trim_end_matches(['\n', '\r']))
    }

    /// Render a complete block of text that was not streamed, such as `run_once`'s final answer.
    pub fn render_block(&mut self, text: &str) -> String {
        let mut out = self.push(text);
        out.push_str(&self.finish());
        out
    }

    fn render_line(&mut self, line: &str) -> String {
        if !self.enabled {
            return line.to_string();
        }
        let trimmed = line.trim_start();

        // A fence toggles code mode; the marker itself is dimmed so the block's edges stay visible
        // without competing with the code inside it.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            self.in_fence = !self.in_fence;
            return format!("{DIM}{line}{RESET}");
        }
        if self.in_fence {
            return format!("{CYAN}{line}{RESET}");
        }
        if is_heading(trimmed) {
            return format!("{BOLD}{line}{RESET}");
        }
        if trimmed.starts_with('>') || is_horizontal_rule(trimmed) {
            return format!("{DIM}{line}{RESET}");
        }
        match split_list_marker(line) {
            Some((marker, rest)) => format!("{BOLD}{marker}{RESET}{}", inline(rest)),
            None => inline(line),
        }
    }
}

/// Render the supported Markdown subset into owned Ratatui spans. This mirrors the streaming ANSI
/// renderer while keeping formatting semantic so the fullscreen transcript can wrap and scroll.
pub fn render_ratatui(text: &str) -> Text<'static> {
    let mut in_fence = false;
    let mut lines = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        } else if in_fence {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default()
                    .fg(Color::Rgb(0x9c, 0xd0, 0xde))
                    .bg(Color::Rgb(0x1a, 0x1a, 0x1a)),
            )));
        } else if is_heading(trimmed) {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        } else if trimmed.starts_with('>') || is_horizontal_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        } else if let Some((marker, rest)) = split_list_marker(line) {
            let mut spans = vec![Span::styled(
                marker.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            spans.extend(ratatui_inline(rest));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(ratatui_inline(line)));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }
    Text::from(lines)
}

/// One piece of an inline-formatted line.
enum Segment<'a> {
    Plain(&'a str),
    Code(&'a str),
    Bold(&'a str),
}

/// Scan inline markdown once. Both renderers walk this, so what counts as a code span or an
/// emphasis marker cannot come to differ between the transcript and plain output -- they were
/// two separate scanners that happened to agree.
fn inline_segments(text: &str) -> Vec<Segment<'_>> {
    let mut segments: Vec<Segment<'_>> = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    let mut plain_start = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'`'
            && let Some(end) = text[index + 1..].find('`').map(|at| index + 1 + at)
        {
            if plain_start < index {
                segments.push(Segment::Plain(&text[plain_start..index]));
            }
            segments.push(Segment::Code(&text[index + 1..end]));
            index = end + 1;
            plain_start = index;
            continue;
        }
        if bytes[index] == b'*'
            && bytes.get(index + 1) == Some(&b'*')
            && let Some(end) = text[index + 2..].find("**").map(|at| index + 2 + at)
        {
            let inner = &text[index + 2..end];
            // Require non-blank, non-padded content, the standard rule that keeps `**` in code or
            // a glob (`src/**/*.rs`) from being read as an emphasis marker.
            if !inner.is_empty()
                && !inner.starts_with(char::is_whitespace)
                && !inner.ends_with(char::is_whitespace)
            {
                if plain_start < index {
                    segments.push(Segment::Plain(&text[plain_start..index]));
                }
                segments.push(Segment::Bold(inner));
                index = end + 2;
                plain_start = index;
                continue;
            }
        }
        let character = text[index..]
            .chars()
            .next()
            .expect("index is on a boundary");
        index += character.len_utf8();
    }
    if plain_start < text.len() {
        segments.push(Segment::Plain(&text[plain_start..]));
    }
    segments
}

fn ratatui_inline(text: &str) -> Vec<Span<'static>> {
    inline_segments(text)
        .into_iter()
        .map(|segment| match segment {
            Segment::Plain(text) => Span::raw(text.to_string()),
            Segment::Code(text) => Span::styled(
                text.to_string(),
                Style::default()
                    .fg(Color::Rgb(0x9c, 0xd0, 0xde))
                    .bg(Color::Rgb(0x1a, 0x1a, 0x1a)),
            ),
            Segment::Bold(text) => Span::styled(
                text.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        })
        .collect()
}

fn is_heading(trimmed: &str) -> bool {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ')
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    let trimmed = trimmed.trim_end();
    trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
}

/// Split a leading list marker (`- `, `* `, `+ `, `1. `) from the rest of the line, keeping the
/// original indentation with the marker so nesting still lines up.
fn split_list_marker(line: &str) -> Option<(&str, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];

    if let Some(after) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        return Some((&line[..indent + 2], after));
    }

    // An ordered marker: digits followed by `. `.
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && rest[digits..].starts_with(". ") {
        let end = indent + digits + 2;
        return Some((&line[..end], &line[end..]));
    }
    None
}

/// Style inline code spans and bold runs, leaving everything else untouched. A code span wins over
/// bold, and its contents are never reinterpreted, so a backticked `**not bold**` stays literal.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in inline_segments(text) {
        match segment {
            Segment::Plain(text) => out.push_str(text),
            Segment::Code(text) => {
                out.push_str(CYAN);
                out.push_str(text);
                out.push_str(RESET);
            }
            Segment::Bold(text) => {
                out.push_str(BOLD);
                out.push_str(text);
                out.push_str(RESET);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str) -> String {
        Renderer::new(true).render_block(text)
    }

    /// The segments a line scans into, as plain markers, so both renderers can be checked
    /// against the same expectation.
    fn segments(text: &str) -> Vec<String> {
        inline_segments(text)
            .into_iter()
            .map(|segment| match segment {
                Segment::Plain(text) => format!("plain:{text}"),
                Segment::Code(text) => format!("code:{text}"),
                Segment::Bold(text) => format!("bold:{text}"),
            })
            .collect()
    }

    #[test]
    fn both_renderers_scan_a_line_the_same_way() {
        // They were two separate scanners that happened to agree. This pins the shared rules:
        // what is code, what is emphasis, and what is left alone.
        assert_eq!(
            segments("a `code` and **bold** end"),
            vec![
                "plain:a ",
                "code:code",
                "plain: and ",
                "bold:bold",
                "plain: end"
            ]
        );
        // A glob is not emphasis, and neither is a padded pair.
        assert_eq!(segments("src/**/*.rs"), vec!["plain:src/**/*.rs"]);
        assert_eq!(segments("a ** b ** c"), vec!["plain:a ** b ** c"]);
        // An unclosed marker stays literal rather than swallowing the rest of the line.
        assert_eq!(segments("half `open"), vec!["plain:half `open"]);
    }

    #[test]
    fn the_two_renderers_agree_on_what_they_emphasise() {
        let line = "a `code` and **bold** end";
        let ansi = inline(line);
        let spans = ratatui_inline(line);
        // Same text either way; only the styling mechanism differs.
        let from_spans: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(from_spans, "a code and bold end");
        assert!(ansi.contains(CYAN), "code is coloured in ANSI too");
        assert!(ansi.contains(BOLD), "and emphasised the same way");
        assert_eq!(spans.len(), 5, "one span per segment: {spans:?}");
    }

    #[test]
    fn disabled_rendering_is_a_passthrough() {
        let text = "# Heading\n**bold** and `code`\n";
        assert_eq!(Renderer::new(false).render_block(text), text);
    }

    #[test]
    fn headings_are_bold() {
        assert_eq!(render("## Title"), format!("{BOLD}## Title{RESET}"));
        // A hash without a space is not a heading (e.g. a comment or an issue reference).
        assert_eq!(render("#notaheading"), "#notaheading");
        assert_eq!(render("####### too many"), "####### too many");
    }

    #[test]
    fn inline_code_and_bold_are_styled() {
        assert_eq!(
            render("use `cargo test` now"),
            format!("use {CYAN}cargo test{RESET} now")
        );
        assert_eq!(
            render("this is **important** here"),
            format!("this is {BOLD}important{RESET} here")
        );
    }

    #[test]
    fn code_spans_are_not_reinterpreted() {
        assert_eq!(render("`**literal**`"), format!("{CYAN}**literal**{RESET}"));
    }

    #[test]
    fn glob_patterns_and_snake_case_survive_untouched() {
        // The two things a naive renderer mangles most often in a coding agent's output.
        assert_eq!(render("src/**/*.rs"), "src/**/*.rs");
        assert_eq!(
            render("call read_project_file here"),
            "call read_project_file here"
        );
        assert_eq!(render("2 * 3 * 4"), "2 * 3 * 4");
    }

    #[test]
    fn unterminated_markers_are_left_alone() {
        assert_eq!(render("an **unclosed run"), "an **unclosed run");
        assert_eq!(render("a `dangling span"), "a `dangling span");
    }

    #[test]
    fn fenced_blocks_are_styled_until_closed() {
        let rendered = render("before\n```rust\nfn main() {}\n```\nafter");
        let lines: Vec<&str> = rendered.split('\n').collect();

        assert_eq!(lines[0], "before");
        assert_eq!(lines[1], format!("{DIM}```rust{RESET}"));
        assert_eq!(lines[2], format!("{CYAN}fn main() {{}}{RESET}"));
        assert_eq!(lines[3], format!("{DIM}```{RESET}"));
        assert_eq!(lines[4], "after");
    }

    #[test]
    fn markdown_inside_a_fence_is_not_styled() {
        let rendered = render("```\n# not a heading\n**not bold**\n```");
        assert!(rendered.contains(&format!("{CYAN}# not a heading{RESET}")));
        assert!(!rendered.contains(BOLD));
    }

    #[test]
    fn list_markers_are_highlighted_and_indentation_is_kept() {
        assert_eq!(render("- first"), format!("{BOLD}- {RESET}first"));
        assert_eq!(
            render("  1. numbered"),
            format!("{BOLD}  1. {RESET}numbered")
        );
        // A bare dash is a horizontal rule, not a bullet.
        assert_eq!(render("---"), format!("{DIM}---{RESET}"));
    }

    #[test]
    fn blockquotes_are_dimmed() {
        assert_eq!(render("> quoted"), format!("{DIM}> quoted{RESET}"));
    }

    #[test]
    fn streaming_in_fragments_matches_rendering_all_at_once() {
        let text =
            "# Title\n\nUse `cargo test` and **check** it.\n\n```rs\nlet x = 1;\n```\n- done\n";
        let expected = Renderer::new(true).render_block(text);

        // Feed the same text one character at a time, the worst case for a line buffer.
        let mut renderer = Renderer::new(true);
        let mut streamed = String::new();
        for character in text.chars() {
            streamed.push_str(&renderer.push(&character.to_string()));
        }
        streamed.push_str(&renderer.finish());

        assert_eq!(streamed, expected);
    }

    #[test]
    fn a_trailing_partial_line_is_flushed_by_finish() {
        let mut renderer = Renderer::new(true);
        assert_eq!(renderer.push("**done**"), "");
        assert_eq!(renderer.finish(), format!("{BOLD}done{RESET}"));
        // A second flush has nothing left to emit.
        assert_eq!(renderer.finish(), "");
    }

    #[test]
    fn carriage_returns_are_stripped_from_line_ends() {
        assert_eq!(Renderer::new(true).render_block("plain\r\n"), "plain\n");
    }

    #[test]
    fn ratatui_renderer_keeps_semantic_markdown_spans() {
        let text =
            render_ratatui("# Title\nUse `cargo test` and **check** it.\n```rs\nlet x = 1;\n```");
        assert_eq!(text.lines.len(), 5);
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            text.lines[1]
                .spans
                .iter()
                .any(|span| span.content == "cargo test")
        );
        assert!(
            text.lines[1]
                .spans
                .iter()
                .any(|span| span.content == "check")
        );
        assert_eq!(text.lines[3].spans[0].content, "let x = 1;");
        assert_eq!(
            text.lines[3].spans[0].style.bg,
            Some(Color::Rgb(0x1a, 0x1a, 0x1a)),
            "fenced code sits on a panel"
        );
    }
}
