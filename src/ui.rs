use crate::terminal::{Style as AnsiStyle, Ui};
use anyhow::{Context, Result};
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    io::{self, Stdout, Write},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Braille spinner frames — the same animation the plain scrollback mode uses.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Welcome logo, opencode-style: block-letter art split into a muted left half and a bright,
/// bold right half (KAM | UI) so the brand pops without shouting. Rendered centered while the
/// transcript has no messages yet; the chat view takes over on the first message.
const LOGO_LEFT: [&str; 6] = [
    "██╗  ██╗  █████╗  ███╗   ███╗",
    "██║ ██╔╝ ██╔══██╗ ████╗ ████║",
    "█████╔╝  ███████║ ██╔████╔██║",
    "██╔═██╗  ██╔══██║ ██║╚██╔╝██║",
    "██║  ██╗ ██║  ██║ ██║ ╚═╝ ██║",
    "╚═╝  ╚═╝ ╚═╝  ╚═╝ ╚═╝     ╚═╝",
];
const LOGO_RIGHT: [&str; 6] = [
    "██╗   ██╗██╗",
    "██║   ██║██║",
    "██║   ██║██║",
    "██║   ██║██║",
    "╚██████╔╝██║",
    " ╚═════╝ ╚═╝",
];

fn lock_screen(screen: &Mutex<FullScreen>) -> MutexGuard<'_, FullScreen> {
    screen.lock().unwrap_or_else(PoisonError::into_inner)
}

const USER_BG: Color = Color::Rgb(36, 59, 104);
const TOOL_BG: Color = Color::Rgb(49, 58, 77);
const OUTPUT_BG: Color = Color::Rgb(35, 45, 61);
const ERROR_BG: Color = Color::Rgb(125, 39, 59);
const NOTICE_FG: Color = Color::Rgb(150, 164, 183);
const PROMPT_BG: Color = Color::Rgb(36, 59, 104);
const MAX_HISTORY_LINES: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CardKind {
    User,
    Tool,
    Output,
    Error,
}

#[derive(Debug, Clone)]
struct Card {
    kind: CardKind,
    title: String,
    body: String,
    collapsed: bool,
}

#[derive(Debug, Clone)]
struct Model {
    header: String,
    cards: Vec<Card>,
    notices: Vec<String>,
    footer: String,
    scroll: u16,
    prompt_visible: bool,
    thinking: Option<(usize, &'static str)>,
    /// True until the first message lands: the home screen shows the centered logo.
    intro: bool,
    /// OpenCode-style right rail: bold keys with muted values (session, model, context…).
    /// `None` hides it entirely (narrow terminals included).
    sidebar: Option<Vec<(String, String)>>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            header: String::from("Kamui"),
            cards: Vec::new(),
            notices: Vec::new(),
            footer: String::from(
                "/ commands · Tab complete · \u{2191} history · Enter send · Ctrl+C cancel",
            ),
            scroll: 0,
            prompt_visible: true,
            thinking: None,
            intro: true,
            sidebar: None,
        }
    }
}

struct FullScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    model: Model,
}

impl FullScreen {
    fn new(header: String) -> Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("could not enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                return Err(error).context("could not create Ratatui terminal");
            }
        };
        if let Err(error) = terminal.clear() {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen);
            return Err(error).context("could not clear terminal");
        }
        let model = Model {
            header,
            ..Model::default()
        };
        let mut screen = Self { terminal, model };
        screen.draw()?;
        Ok(screen)
    }

    fn draw(&mut self) -> Result<()> {
        let model = self.model.clone();
        self.terminal.draw(|frame| render(frame, &model))?;
        Ok(())
    }

    fn set_header(&mut self, header: String) -> Result<()> {
        self.model.header = header;
        self.draw()
    }

    fn add_card(
        &mut self,
        kind: CardKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<()> {
        self.model.intro = false;
        let title = title.into();
        let body = body.into();
        // Long tool output starts collapsed so a 20k-char dump never floods the transcript;
        // `/expand` opens it. Assistant answers and errors always show.
        let collapsed =
            kind == CardKind::Output && title != "Assistant" && body.lines().count() > 5;
        self.model.cards.push(Card {
            kind,
            title,
            body,
            collapsed,
        });
        self.trim_history();
        self.draw()
    }

    fn update_assistant(&mut self, body: String) -> Result<()> {
        self.model.intro = false;
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Output) && card.title == "Assistant" => {
                card.body = body;
            }
            _ => self.model.cards.push(Card {
                kind: CardKind::Output,
                title: "Assistant".to_string(),
                body,
                collapsed: false,
            }),
        }
        self.trim_history();
        self.draw()
    }

    fn set_last_collapsed(&mut self, collapsed: bool) -> Result<bool> {
        let Some(card) = self.model.cards.last_mut() else {
            return Ok(false);
        };
        card.collapsed = collapsed;
        self.draw()?;
        Ok(true)
    }

    fn add_notice(&mut self, text: impl Into<String>) -> Result<()> {
        self.model.notices.push(text.into());
        if self.model.notices.len() > 32 {
            self.model.notices.remove(0);
        }
        self.draw()
    }

    fn prompt(&mut self) -> Result<()> {
        self.model.prompt_visible = true;
        self.draw()
    }

    fn trim_history(&mut self) {
        let mut line_count = 0usize;
        for card in self.model.cards.iter().rev() {
            line_count += card.body.lines().count() + 3;
            if line_count > MAX_HISTORY_LINES {
                break;
            }
        }
        if self.model.cards.len() > MAX_HISTORY_LINES {
            let keep_from = self.model.cards.len().saturating_sub(MAX_HISTORY_LINES);
            self.model.cards.drain(..keep_from);
        }
    }
}

impl Drop for FullScreen {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.backend_mut().flush();
    }
}

/// Unified output surface. Plain mode preserves the existing scrollback contract; interactive
/// mode uses a retained Ratatui transcript while the agent loop remains provider-agnostic.
/// The fullscreen terminal sits behind a mutex so the thinking-animation ticker can redraw it
/// from a background task while the chat loop is blocked awaiting the model stream.
pub struct ChatUi {
    plain: Ui,
    fullscreen: Option<Arc<Mutex<FullScreen>>>,
    thinking: Option<ThinkingHandle>,
}

struct ThinkingHandle {
    stop: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ThinkingHandle {
    fn drop(&mut self) {
        self.stop.notify_one();
        self.task.abort();
    }
}

impl ChatUi {
    pub fn new(interactive: bool, header: String) -> Result<Self> {
        let plain = Ui::stdio();
        let fullscreen = interactive
            .then(|| FullScreen::new(header).map(|screen| Arc::new(Mutex::new(screen))))
            .transpose()?;
        Ok(Self {
            plain,
            fullscreen,
            thinking: None,
        })
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    fn screen(&self) -> MutexGuard<'_, FullScreen> {
        lock_screen(
            self.fullscreen
                .as_ref()
                .expect("fullscreen surface must exist"),
        )
    }

    /// Start the frea-style loading animation in the footer while the model thinks. No-op outside
    /// fullscreen mode; plain mode keeps its inline spinner.
    pub fn thinking_start(&mut self, label: &'static str) -> Result<()> {
        if !self.is_fullscreen() || self.thinking.is_some() {
            return Ok(());
        }
        {
            let mut screen = self.screen();
            screen.model.thinking = Some((0, label));
            screen.draw()?;
        }
        let shared = self.fullscreen.clone().expect("fullscreen surface");
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_task = stop.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(80));
            loop {
                tokio::select! {
                    _ = stop_task.notified() => break,
                    _ = interval.tick() => {
                        let mut screen = lock_screen(&shared);
                        if let Some((frame, _)) = screen.model.thinking.as_mut() {
                            *frame = frame.wrapping_add(1);
                            let _ = screen.draw();
                        } else {
                            break;
                        }
                    }
                }
            }
            let mut screen = lock_screen(&shared);
            screen.model.thinking = None;
            let _ = screen.draw();
        });
        self.thinking = Some(ThinkingHandle { stop, task });
        Ok(())
    }

    /// Stop the loading animation and leave the footer clean for the next prompt.
    pub async fn thinking_stop(&mut self) {
        if let Some(mut handle) = self.thinking.take() {
            handle.stop.notify_one();
            let task = std::mem::replace(&mut handle.task, tokio::spawn(async {}));
            let _ = task.await;
        }
    }

    /// Replace the right-rail contents (opencode-style session info). Hidden on narrow
    /// terminals; no-op outside fullscreen mode.
    pub fn set_sidebar(&mut self, entries: Vec<(String, String)>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.sidebar = Some(entries);
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_header(&mut self, header: String) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_header(header),
            None => {
                print!("{header}");
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn prompt(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).prompt(),
            None => {
                print!(
                    "{}",
                    self.plain
                        .style("\u{276f} ", &[AnsiStyle::Cyan, AnsiStyle::Bold])
                );
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn user(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::User, "User", text)
    }

    pub fn tool_call(&mut self, name: &str, args: &str) -> Result<()> {
        self.card(CardKind::Tool, format!("Tool: {name}"), args)
    }

    pub fn tool_output(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::Output, "Tool Output", text)
    }

    pub fn tool_error(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::Error, "Error", text)
    }

    pub fn expand_last(&mut self) -> Result<bool> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_last_collapsed(false),
            None => Ok(false),
        }
    }

    pub fn collapse_last(&mut self) -> Result<bool> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).set_last_collapsed(true),
            None => Ok(false),
        }
    }

    pub fn assistant_update(&mut self, raw_markdown: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).update_assistant(raw_markdown.to_string()),
            None => {
                let mut renderer = crate::markdown::Renderer::for_stdout();
                print!("{}", renderer.render_block(raw_markdown));
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn assistant_done(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).draw(),
            None => {
                println!();
                Ok(())
            }
        }
    }

    pub fn notice(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_notice(text),
            None => {
                println!("{text}");
                Ok(())
            }
        }
    }

    pub fn warning(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_notice(format!("WARNING: {text}")),
            None => {
                print!("{}", crate::render::render_warning(text, self.plain));
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    pub fn error(&mut self, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_card(CardKind::Error, "Error", text),
            None => {
                eprintln!(
                    "{}",
                    self.plain
                        .tool_outcome(&format!("Error: {text}"), Duration::ZERO)
                );
                Ok(())
            }
        }
    }

    fn card(
        &mut self,
        kind: CardKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_card(kind, title, body),
            None => {
                let title = title.into();
                let body = body.into();
                let rendered = match kind {
                    CardKind::User => crate::render::render_user_prompt(&body, self.plain),
                    CardKind::Tool => {
                        crate::render::render_tool_call(&title[6..], &body, self.plain)
                    }
                    CardKind::Output => crate::render::render_tool_output(&body, self.plain),
                    CardKind::Error => crate::render::render_error(&body, self.plain),
                };
                print!("{rendered}");
                Ok(())
            }
        }
    }
}

fn render(frame: &mut Frame<'_>, model: &Model) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let header = Paragraph::new(header_line(&model.header)).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .title(" \u{25c6} Kamui ")
            .title_style(
                Style::default()
                    .fg(Color::Rgb(97, 175, 239))
                    .add_modifier(Modifier::BOLD),
            ),
    );
    frame.render_widget(header, areas[0]);

    // OpenCode-style split: the sidebar rail takes the right side once the terminal is wide
    // enough and a session is actually underway.
    let body_area = areas[1];
    let (transcript_area, sidebar_area) = match (&model.sidebar, model.intro) {
        (Some(entries), false) if !entries.is_empty() && body_area.width >= 84 => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(30)])
                .split(body_area);
            (cols[0], Some(cols[1]))
        }
        _ => (body_area, None),
    };

    if model.intro && model.cards.is_empty() {
        frame.render_widget(intro_paragraph(model, body_area), body_area);
    } else {
        let transcript = transcript_text(model, transcript_area.width);
        let transcript_height = transcript.lines.len().saturating_add(1) as u16;
        let visible = transcript_area.height.saturating_sub(2);
        let scroll = transcript_height.saturating_sub(visible);
        let body = Paragraph::new(transcript)
            .wrap(Wrap { trim: false })
            .scroll((scroll.max(model.scroll), 0))
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT));
        frame.render_widget(body, transcript_area);
    }
    if let Some(area) = sidebar_area {
        frame.render_widget(sidebar_paragraph(model, area), area);
    }

    let footer = footer_widget(model);
    frame.render_widget(footer, areas[2]);
}

/// Session-info rail: each entry renders its key in bold with the value muted beneath,
/// sections separated by blank lines — the same visual grammar as opencode's sidebar.
fn sidebar_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(entries) = &model.sidebar {
        for (key, value) in entries {
            lines.push(Line::from(Span::styled(
                format!("{key} "),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for value_line in wrap_display(value, area.width.saturating_sub(4) as usize) {
                lines.push(Line::styled(value_line, Style::default().fg(NOTICE_FG)));
            }
            lines.push(Line::from(""));
        }
    }
    Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::LEFT))
}

/// The home screen: two-tone block-letter logo centered above the version/model line and the
/// getting-started hint. Mirrors opencode's muted-left / bright-right treatment.
fn intro_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let width = area.width.max(20) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total_height = LOGO_LEFT.len() + 4; // gap above + logo + blank + info + hint
    let top_pad = area.height.saturating_sub(total_height as u16 + 4) as usize / 3;
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }
    for (left, right) in LOGO_LEFT.iter().zip(LOGO_RIGHT.iter()) {
        let combined = format!("{left}{right}");
        let pad = width.saturating_sub(combined.chars().count()) / 2;
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled((*left).to_string(), Style::default().fg(NOTICE_FG)),
            Span::styled(
                (*right).to_string(),
                Style::default()
                    .fg(Color::Rgb(97, 175, 239))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    let info = model.header.clone();
    let pad = width.saturating_sub(UnicodeWidthStr::width(info.as_str())) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(info, Style::default().fg(NOTICE_FG)),
    ]));
    let hint = "type a message, or / for commands";
    let pad = width.saturating_sub(hint.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            hint.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    // Startup notices (warnings, hints) still belong on the home screen — the logo must never
    // hide them.
    for notice in &model.notices {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("\u{2500}\u{2500}\u{2500} {notice} "),
            Style::default().fg(NOTICE_FG),
        ));
    }
    Paragraph::new(Text::from(lines))
}

/// "Kamui v… · model · path" with the meta half dimmed so the brand reads first.
fn header_line(header: &str) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some((brand, rest)) = header.split_once(" \u{b7} ") {
        spans.push(Span::styled(
            format!("{brand} "),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("\u{b7} {rest}"),
            Style::default().fg(NOTICE_FG),
        ));
    } else {
        spans.push(Span::styled(
            header.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// Hint line plus the prompt row: the frea spinner takes over while thinking, otherwise the
/// cyan `❯` marks where typing lands.
fn footer_widget(model: &Model) -> Paragraph<'static> {
    let hints = Line::styled(model.footer.clone(), Style::default().fg(NOTICE_FG));
    let prompt_line = match model.thinking {
        Some((frame, label)) => Line::from(vec![
            Span::styled(
                format!("{} ", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]),
                Style::default()
                    .fg(Color::Rgb(97, 175, 239))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(NOTICE_FG)),
        ]),
        None => {
            if model.prompt_visible {
                Line::from(vec![
                    Span::styled(
                        "\u{276f} ".to_string(),
                        Style::default()
                            .fg(Color::Rgb(97, 175, 239))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "type a message, or / for commands",
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ])
            } else {
                Line::from("")
            }
        }
    };
    Paragraph::new(Text::from(vec![hints, prompt_line]))
        .style(Style::default().bg(PROMPT_BG))
        .block(Block::default().borders(Borders::TOP))
}

fn transcript_text(model: &Model, width: u16) -> Text<'static> {
    let inner_width = width.saturating_sub(2).max(20) as usize;
    let mut lines = Vec::new();
    for card in &model.cards {
        lines.extend(card_lines(card, inner_width));
        lines.push(Line::from(""));
    }
    for notice in &model.notices {
        lines.push(Line::styled(
            format!("─── {notice} "),
            Style::default().fg(NOTICE_FG),
        ));
    }
    Text::from(lines)
}

fn card_lines(card: &Card, width: usize) -> Vec<Line<'static>> {
    // OpenCode-style transcript: only input, tool calls, and errors get blocks; streamed answers
    // read as clean markdown with a slim accent bar, and raw tool output stays dim and quiet.
    if card.title == "Assistant" {
        return assistant_lines(card, width);
    }
    if card.kind == CardKind::Output {
        let mut out = Vec::new();
        for line in wrap_display(&card.body, width.saturating_sub(4)) {
            out.push(filled_line(
                format!("  \u{b7} {line}"),
                width,
                Style::default().fg(NOTICE_FG),
            ));
        }
        return out;
    }
    let style = match card.kind {
        CardKind::User => Style::default().bg(USER_BG).fg(Color::White),
        CardKind::Tool => Style::default().bg(TOOL_BG).fg(Color::White),
        CardKind::Output => Style::default().bg(OUTPUT_BG).fg(Color::White),
        CardKind::Error => Style::default().bg(ERROR_BG).fg(Color::White),
    };
    let title_style = style.add_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    let title = format!(
        "{} {} ",
        match card.kind {
            CardKind::User => "\u{25cf}",
            CardKind::Tool => "\u{25b8}",
            _ => "",
        },
        card.title
    );
    let top = format!(
        "{title}{}",
        "\u{2500}".repeat(width.saturating_sub(UnicodeWidthStr::width(title.as_str())))
    );
    out.push(filled_line(top, width, title_style));
    let total_lines = card.body.lines().count();
    if card.collapsed {
        // Head preview plus the tail count, so a collapsed card still says what's inside.
        for line in wrap_display(
            card.body.lines().next().unwrap_or(""),
            width.saturating_sub(4),
        )
        .into_iter()
        .take(1)
        {
            out.push(filled_line(format!("  {line}"), width, style));
        }
        out.push(filled_line(
            format!(
                "  \u{2026} {} more line(s) — /expand to show",
                total_lines.saturating_sub(1)
            ),
            width,
            style,
        ));
    } else {
        for line in wrap_display(&card.body, width.saturating_sub(4)) {
            out.push(filled_line(format!("  {line}"), width, style));
        }
    }
    out.push(filled_line("\u{2500}".repeat(width), width, style));
    out
}

/// Assistant messages render as plain markdown behind a slim accent bar — no full-width
/// background box, so long answers stay comfortable to read.
fn assistant_lines(card: &Card, _width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if card.collapsed {
        out.push(Line::styled(
            "  \u{2026} (collapsed; press Enter to expand)".to_string(),
            Style::default().fg(NOTICE_FG),
        ));
        return out;
    }
    let accent = Style::default().fg(Color::Rgb(97, 175, 239));
    let text = crate::markdown::render_ratatui(&card.body);
    for line in text.lines {
        let mut spans = vec![Span::styled("\u{258d} ".to_string(), accent)];
        spans.extend(
            line.spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), span.style)),
        );
        out.push(Line::from(spans));
    }
    out
}

fn filled_line(text: String, width: usize, style: Style) -> Line<'static> {
    filled_spans(vec![Span::styled(text, style)], width, style)
}

fn filled_spans(mut spans: Vec<Span<'static>>, width: usize, style: Style) -> Line<'static> {
    let current: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if current < width {
        spans.push(Span::styled(" ".repeat(width - current), style));
    }
    Line::from(spans)
}

fn wrap_display(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut result = Vec::new();
    for source in text.lines() {
        if source.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in source.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width > 0 && current_width + char_width > width {
                result.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += char_width;
        }
        result.push(current);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_uses_display_width() {
        let rows = wrap_display("abc界def", 5);
        assert_eq!(rows, vec!["abc界", "def"]);
    }

    #[test]
    fn card_lines_have_a_box_shape() {
        let card = Card {
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            collapsed: false,
        };
        let lines = card_lines(&card, 20);
        assert_eq!(lines.len(), 3);
        let width: usize = lines[0]
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        assert_eq!(width, 20);
    }

    #[test]
    fn assistant_cards_render_unboxed() {
        let card = Card {
            kind: CardKind::Output,
            title: "Assistant".into(),
            body: "hello **world**".into(),
            collapsed: false,
        };
        let lines = card_lines(&card, 40);
        assert!(!lines.is_empty());
        // No full-width background fill: lines end where the text ends, and each starts with
        // the accent bar instead of a box border.
        for line in &lines {
            assert_eq!(line.spans[0].content, "\u{258d} ");
            assert!(line.spans[0].style.bg.is_none());
        }
    }
}
