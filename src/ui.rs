use crate::terminal::{Style as AnsiStyle, Ui};
use anyhow::{Context, Result};
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{
    collections::VecDeque,
    io::{self, Stdout, Write},
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::Duration,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Braille spinner frames — the same animation the plain scrollback mode uses.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Bouncing wall for the input editor while the agent is thinking.
/// A 3-cell bright wall (███) glides on a muted track (─), pausing
/// briefly at each end for eased ping-pong. A one-cell fade tail (▓)
/// trails behind the direction of travel for a subtle motion-blur.
const WALL_TRACK_LEN: usize = 10;
const WALL_LEN: usize = 3;
const WALL_POS: [usize; 16] = [0, 0, 1, 2, 3, 4, 5, 6, 7, 7, 6, 5, 4, 3, 2, 1];

fn bouncing_wall_spans(frame_idx: usize) -> Vec<Span<'static>> {
    let pos = WALL_POS[frame_idx % WALL_POS.len()];
    // first half of the cycle moves right, second half moves left
    let moving_right = (frame_idx % WALL_POS.len()) < 8;
    let mut spans = Vec::with_capacity(WALL_TRACK_LEN + 2);
    spans.push(Span::styled("[", Style::default().fg(MUTED)));
    for i in 0..WALL_TRACK_LEN {
        let in_wall = i >= pos && i < pos + WALL_LEN;
        let is_tail = if moving_right {
            pos > 0 && i + 1 == pos
        } else {
            i == pos + WALL_LEN && i < WALL_TRACK_LEN
        };
        if in_wall {
            spans.push(Span::styled(
                "█",
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ));
        } else if is_tail {
            spans.push(Span::styled("▓", Style::default().fg(BLUE)));
        } else {
            spans.push(Span::styled("─", Style::default().fg(BORDER)));
        }
    }
    spans.push(Span::styled("]", Style::default().fg(MUTED)));
    spans
}

// kept for tests / plain-mode fallback — now consistent width, thin track
#[allow(dead_code)]
const BOUNCING_WALL_FRAMES: [&str; 16] = [
    "[███───────]",
    "[███───────]",
    "[─███──────]",
    "[──███─────]",
    "[───███────]",
    "[────███───]",
    "[─────███──]",
    "[──────███─]",
    "[───────███]",
    "[───────███]",
    "[──────███─]",
    "[─────███──]",
    "[────███───]",
    "[───███────]",
    "[──███─────]",
    "[─███──────]",
];

struct WrappedCache {
    width: u16,
    fp: u64,
    rows: Vec<(Line<'static>, Option<u64>)>,
}

static WRAPPED_CACHE: OnceLock<Mutex<Option<WrappedCache>>> = OnceLock::new();

fn wrapped_fingerprint(model: &Model) -> u64 {
    let mut fp: u64 = 146959;
    fp = fp.wrapping_mul(31).wrapping_add(model.cards.len() as u64);
    for card in &model.cards {
        fp = fp.wrapping_mul(31).wrapping_add(card.id);
        fp = fp.wrapping_mul(31).wrapping_add(card.body.len() as u64);
        fp = fp.wrapping_mul(31).wrapping_add(card.title.len() as u64);
        fp = fp.wrapping_mul(31).wrapping_add(card.collapsed as u64);
        fp = fp.wrapping_mul(31).wrapping_add(match card.kind {
            CardKind::User => 1,
            CardKind::Tool => 2,
            CardKind::Output => 3,
            CardKind::Error => 4,
            CardKind::Note => 5,
        });
        if let Some((status, ok)) = &card.status {
            fp = fp.wrapping_mul(31).wrapping_add(status.len() as u64);
            fp = fp.wrapping_add(*ok as u64 + 7);
        }
    }
    if let Some((frame, _)) = model.thinking {
        fp = fp.wrapping_mul(31).wrapping_add(frame as u64 + 11);
    }
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warnings.len() as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warnings_visible as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warning_details.len() as u64);
    fp = fp
        .wrapping_mul(31)
        .wrapping_add(model.warning_details_visible as u64);
    fp
}

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

/// Smaller KAMUI for the exit screen — 4 rows, flat (no shadow)
/// so the sign-off stays crisp and clearly reads KAMUI.
pub(crate) const EXIT_LOGO_SMALL: [&str; 4] = [
    "██  ██   ███    █   █   █   █   ███ ",
    "██ ██   █   █   ██ ██   █   █    █  ",
    "████    █████   █ █ █   █   █    █  ",
    "██  ██  █   █   █   █    ███    ███ ",
];

fn lock_screen(screen: &Mutex<FullScreen>) -> MutexGuard<'_, FullScreen> {
    screen.lock().unwrap_or_else(PoisonError::into_inner)
}

// OpenCode dark palette (theme/assets/opencode.json) with kamui's blue as the brand accent.
const TEXT: Color = Color::Rgb(0xee, 0xee, 0xee);
const MUTED: Color = Color::Rgb(0x80, 0x80, 0x80);
const BORDER: Color = Color::Rgb(0x48, 0x48, 0x48);
const BG_ELEMENT: Color = Color::Rgb(0x1e, 0x1e, 0x1e);
const BG_PANEL: Color = Color::Rgb(0x14, 0x14, 0x14);
const BLUE: Color = Color::Rgb(0x5c, 0x9c, 0xf5);
/// Opaque near-black for every overlay so nothing bleeds through.
const POPUP_BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a);
/// Search hits: every match gets a quiet wash, the one in view a brighter one.
const MATCH_BG: Color = Color::Rgb(0x33, 0x2d, 0x14);
const MATCH_CURRENT_BG: Color = Color::Rgb(0x6b, 0x55, 0x12);
/// Warm accent marking the user's own words (opencode primary tone).
const ACCENT: Color = Color::Rgb(0xfa, 0xb2, 0x83);
const GREEN: Color = Color::Rgb(0x7f, 0xd8, 0x8f);
const WARN: Color = Color::Rgb(0xf5, 0xa7, 0x42);
const RED: Color = Color::Rgb(0xe0, 0x6c, 0x75);
/// Kept from the earlier blue scheme; NOTICE_FG aliases it for readability everywhere.
const NOTICE_FG: Color = MUTED;
const MAX_HISTORY_LINES: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CardKind {
    User,
    Tool,
    Output,
    Error,
    /// Command output and status lines. These used to live in a separate six-entry ring that
    /// rendered below every card, so they lost their place in the conversation and older ones
    /// silently fell off the end.
    Note,
}

#[derive(Debug, Clone)]
struct Card {
    /// Monotonic id. Clicks resolve to a card through this rather than a position, so a
    /// history trim between the draw and the click cannot toggle the wrong card.
    id: u64,
    kind: CardKind,
    title: String,
    body: String,
    /// Tool outcome ("completed - 1.2s - 142 chars") plus whether it succeeded. Rendered as
    /// its own row that stays visible when the card is folded, so a finished tool always
    /// reports how it ended without the output being unfolded.
    status: Option<(String, bool)>,
    collapsed: bool,
}

impl Card {
    /// What copying this cell yields. An answer copies as the raw Markdown that was streamed,
    /// with no rails or headers; anything else keeps its header and outcome, which are the
    /// parts that say what the body actually is.
    fn clipboard_text(&self) -> String {
        if self.title == "Assistant" {
            return self.body.clone();
        }
        let mut out = String::new();
        if !self.title.is_empty() {
            out.push_str(&self.title);
            out.push('\n');
        }
        if let Some((status, _)) = &self.status {
            out.push_str(status);
            out.push('\n');
        }
        out.push_str(&self.body);
        out
    }

    /// Body rows hidden behind the fold; 0 means there is nothing to expand.
    fn foldable_rows(&self) -> usize {
        if self.body.trim().is_empty() {
            0
        } else {
            self.body.lines().count()
        }
    }
}

#[derive(Debug, Clone)]
struct Model {
    header: String,
    cards: Vec<Card>,
    footer: String,
    /// Transcript viewport offset in wrapped rows counted from the bottom; 0 means "follow
    /// the tail". PageUp/PageDown/Home/End move it, typing snaps back.
    scroll_from_bottom: usize,
    prompt_visible: bool,
    thinking: Option<(usize, &'static str)>,
    /// True until the first message lands: the home screen shows the centered logo.
    intro: bool,
    /// OpenCode-style right rail: bold keys with muted values (session, model, context…).
    /// `None` hides it entirely (narrow terminals included).
    sidebar: Option<Vec<(String, String)>>,
    /// Live typed text rendered inside the editor box; driven by `ScreenHandle`.
    input: String,
    /// Caret position as a byte offset into `input`. Editing happens here, not only at the end.
    input_caret: usize,
    /// Autocomplete menu state mirrored from the input loop each keystroke.
    ac_items: Vec<(String, String)>,
    ac_selected: usize,
    /// Warning messages render separately so `/warnings` can hide or reveal them.
    warnings: Vec<String>,
    warnings_visible: bool,
    warning_details: Vec<String>,
    warning_details_visible: bool,
    /// Lines typed while the agent runs; shown in the footer until consumed.
    queued_count: usize,
    /// Open modal (model picker / session switcher), opencode-style.
    dialog: Option<DialogState>,
    /// `?` overlay with keybindings.
    help_visible: bool,
    /// Right-side status-bar badge ("5.9k tok 41%", amber past 80%).
    token_badge: Option<(String, u8)>,
    /// Open approval modal (opencode permission panel).
    permission: Option<PermissionState>,
    /// Hidden by the user with Ctrl+B, as opposed to dropped for want of room.
    sidebar_hidden: bool,
    /// Live transcript search (Ctrl+F). `/search` looks through saved sessions in SQLite; this
    /// looks through what is on screen right now, which is a different question.
    search: Option<SearchState>,
}

/// An in-progress transcript search: what was typed, and which match is being looked at.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    /// Index into the matching rows, wrapping at both ends.
    pub current: usize,
    /// Match count from the last draw, for the `3/17` readout.
    pub total: usize,
}

/// Approval modal options, opencode labels.
pub const PERM_OPTIONS: [(&str, &str); 3] = [
    ("y", "Allow once"),
    ("a", "Always allow this session"),
    ("n", "Reject"),
];

#[derive(Debug, Clone)]
pub struct PermissionState {
    pub title: String,
    pub body: String,
    pub selected: usize,
    /// First body row shown. A patch diff is routinely longer than the modal, and the body was
    /// cut at ten rows with nothing saying so -- you were being asked to authorise a change you
    /// could not finish reading.
    pub scroll: usize,
}

/// A modal picker that submits an existing slash command on Enter — pure UI sugar over
/// `/model <name>` and `/resume <id>`, exactly like opencode's model/session dialogs.
#[derive(Debug, Clone)]
pub struct DialogState {
    pub title: String,
    pub prefix: String,
    pub items: Vec<(String, String)>,
    pub query: String,
    pub selected: usize,
}

impl DialogState {
    pub fn new(title: &str, prefix: &str, items: Vec<(String, String)>) -> Self {
        Self {
            title: title.to_string(),
            prefix: prefix.to_string(),
            items,
            query: String::new(),
            selected: 0,
        }
    }

    pub fn filtered(&self) -> Vec<&(String, String)> {
        let needle = self.query.to_ascii_lowercase();
        self.items
            .iter()
            .filter(|(value, label)| {
                value.to_ascii_lowercase().contains(&needle)
                    || label.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }
}

impl Default for Model {
    fn default() -> Self {
        Self {
            header: String::from("Kamui"),
            cards: Vec::new(),
            footer: String::from(
                "! shell · / commands · Tab · \u{2191} history · Ctrl+O expand · Ctrl+Y copy · PgUp/PgDn scroll · Ctrl+C cancel",
            ),
            scroll_from_bottom: 0,
            prompt_visible: true,
            thinking: None,
            intro: true,
            sidebar: None,
            input: String::new(),
            input_caret: 0,
            ac_items: Vec::new(),
            ac_selected: 0,
            warnings: Vec::new(),
            warnings_visible: true,
            warning_details: Vec::new(),
            warning_details_visible: false,
            queued_count: 0,
            dialog: None,
            help_visible: false,
            token_badge: None,
            permission: None,
            sidebar_hidden: false,
            search: None,
        }
    }
}

struct FullScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    model: Model,
    /// Transcript viewport height from the most recent draw; PageUp/PageDown use it as the
    /// scroll page size.
    last_viewport_rows: usize,
    /// Terminal row -> card id from the most recent draw, so a mouse click can find the card
    /// under the pointer.
    last_card_rows: Vec<(u16, u64)>,
    /// Transcript width and height from the last draw. Search re-wraps the transcript exactly
    /// as the renderer did, so the row it scrolls to is the row the user will see.
    last_transcript_width: u16,
    next_card_id: u64,
    /// Set once the real terminal has been handed back. Further draws are dropped so a
    /// still-running input thread cannot repaint over restored scrollback.
    restored: bool,
}

impl FullScreen {
    fn new(header: String) -> Result<Self> {
        // If anything panics mid-draw, still restore the terminal instead of leaving it raw.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                SetCursorStyle::DefaultUserShape,
                DisableBracketedPaste,
                LeaveAlternateScreen,
                DisableMouseCapture
            );
            previous_hook(info);
        }));
        let mut stdout = io::stdout();
        enable_raw_mode().context("could not enable raw mode")?;
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            SetCursorStyle::BlinkingBar
        )
        .context("could not enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                return Err(error).context("could not create Ratatui terminal");
            }
        };
        if let Err(error) = terminal.clear() {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
            let _ = disable_raw_mode();
            return Err(error).context("could not clear terminal");
        }
        let model = Model {
            header,
            ..Model::default()
        };
        let mut screen = Self {
            terminal,
            model,
            last_viewport_rows: 0,
            last_card_rows: Vec::new(),
            last_transcript_width: 0,
            next_card_id: 0,
            restored: false,
        };
        screen.draw()?;
        Ok(screen)
    }

    fn draw(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let model = self.model.clone();
        let mut info = RenderInfo::default();
        self.terminal.draw(|frame| {
            info = render(frame, &model);
        })?;
        self.last_viewport_rows = info.viewport_rows;
        self.last_card_rows = info.card_rows;
        self.last_transcript_width = info.transcript_width;
        Ok(())
    }

    fn take_card_id(&mut self) -> u64 {
        self.next_card_id += 1;
        self.next_card_id
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
        if !matches!(kind, CardKind::Note) {
            self.model.intro = false;
        }
        let title = title.into();
        let body = body.into();
        // Agent noise starts folded: every tool call collapses to a two-line peek, and any
        // output longer than two lines joins it. Ctrl+O, a click, or `/expand` // `/collapse`
        // toggle it; answers and errors always show in full.
        let collapsed = match kind {
            CardKind::Tool => true,
            CardKind::Output => title != "Assistant" && body.lines().count() > 2,
            _ => false,
        };
        let id = self.take_card_id();
        self.model.cards.push(Card {
            id,
            kind,
            title,
            body,
            status: None,
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
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: CardKind::Output,
                    title: "Assistant".to_string(),
                    body,
                    status: None,
                    collapsed: false,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Records a tool's outcome. It lands on the pending `Tool` card when there is one, so a
    /// call and its result render as a single block; otherwise it becomes a card of its own.
    fn finish_tool(&mut self, outcome: String, ok: bool, body: String) -> Result<()> {
        self.model.intro = false;
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Tool) && card.status.is_none() => {
                // The call's arguments were the peek while it ran; real output replaces them,
                // and a silent tool keeps its arguments so the card is not left blank.
                if !body.is_empty() {
                    card.body = body;
                }
                card.status = Some((outcome, ok));
                card.collapsed = true;
            }
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: if ok {
                        CardKind::Output
                    } else {
                        CardKind::Error
                    },
                    title: "Tool Output".to_string(),
                    body,
                    status: Some((outcome, ok)),
                    collapsed: true,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Flips the fold on the newest card that has anything folded away.
    fn toggle_last_card(&mut self) -> Result<bool> {
        let Some(card) = self.last_foldable_mut() else {
            return Ok(false);
        };
        card.collapsed = !card.collapsed;
        self.draw()?;
        Ok(true)
    }

    /// Opens transcript search, closing the slash menu so the two never share the row.
    fn open_search(&mut self) -> Result<()> {
        self.model.search = Some(SearchState::default());
        self.model.ac_items.clear();
        self.draw()
    }

    fn close_search(&mut self) -> Result<()> {
        self.model.search = None;
        // Searching scrolls back through history; closing it returns to the live tail, which is
        // where the next answer will appear.
        self.model.scroll_from_bottom = 0;
        self.draw()
    }

    /// Applies an edit to the query and re-runs it from the top.
    fn edit_search(&mut self, edit: impl FnOnce(&mut String)) -> Result<()> {
        let Some(search) = self.model.search.as_mut() else {
            return Ok(());
        };
        edit(&mut search.query);
        search.current = 0;
        self.refresh_search()
    }

    /// Steps to the next or previous match, wrapping at both ends.
    fn step_search(&mut self, delta: isize) -> Result<()> {
        let Some(search) = self.model.search.as_mut() else {
            return Ok(());
        };
        if search.total > 0 {
            let total = search.total as isize;
            let current = search.current as isize;
            search.current = ((current + delta).rem_euclid(total)) as usize;
        }
        self.refresh_search()
    }

    /// Recounts the matches and scrolls the current one into view. The transcript is re-wrapped
    /// exactly as the renderer wraps it, so the row counted here is the row that gets drawn.
    fn refresh_search(&mut self) -> Result<()> {
        let width = self.last_transcript_width;
        let visible = self.last_viewport_rows.max(1);
        let Some(query) = self.model.search.as_ref().map(|s| s.query.clone()) else {
            return Ok(());
        };
        let rows = wrapped_transcript(&self.model, width);
        let hits = matching_rows(&rows, &query);
        if let Some(search) = self.model.search.as_mut() {
            search.total = hits.len();
            if hits.is_empty() {
                search.current = 0;
            } else {
                search.current %= hits.len();
            }
        }
        if let Some(row) = self
            .model
            .search
            .as_ref()
            .filter(|_| !hits.is_empty())
            .map(|search| hits[search.current])
        {
            self.model.scroll_from_bottom = scroll_to_row(rows.len(), visible, row);
        }
        self.draw()
    }

    /// Copies the cell drawn at `row` to the system clipboard, reporting what was taken.
    /// Mouse capture means the terminal's own drag-select is unavailable, so the transcript
    /// needs its own way to get text out.
    fn copy_card_at_row(&mut self, row: u16) -> Result<()> {
        let id = self
            .last_card_rows
            .iter()
            .find(|(y, _)| *y == row)
            .map(|(_, id)| *id);
        let text = id
            .and_then(|id| self.model.cards.iter().find(|card| card.id == id))
            .map(Card::clipboard_text);
        self.copy_reporting(text, "cell")
    }

    /// Copies the newest answer, or failing that the newest cell with any text in it.
    fn copy_latest(&mut self) -> Result<()> {
        let newest_answer = self
            .model
            .cards
            .iter()
            .rev()
            .find(|card| card.title == "Assistant" && !card.body.trim().is_empty());
        let (text, what) = match newest_answer {
            Some(card) => (Some(card.clipboard_text()), "answer"),
            None => (
                self.model
                    .cards
                    .iter()
                    .rev()
                    .find(|card| !card.clipboard_text().trim().is_empty())
                    .map(Card::clipboard_text),
                "cell",
            ),
        };
        self.copy_reporting(text, what)
    }

    fn copy_reporting(&mut self, text: Option<String>, what: &str) -> Result<()> {
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return self.add_notice(format!("nothing to copy under the {what}"));
        };
        let characters = text.chars().count();
        match set_clipboard_text(&text) {
            Ok(()) => self.add_notice(format!("copied {characters} chars ({what}) to clipboard")),
            Err(error) => self.add_notice(format!("could not copy: {error:#}")),
        }
    }

    /// Flips the fold on whichever card was drawn at `row`, for click-to-expand.
    fn toggle_card_at_row(&mut self, row: u16) -> Result<bool> {
        let Some((_, id)) = self.last_card_rows.iter().find(|(y, _)| *y == row).copied() else {
            return Ok(false);
        };
        let Some(card) = self.model.cards.iter_mut().find(|card| card.id == id) else {
            return Ok(false);
        };
        if card.foldable_rows() == 0 {
            return Ok(false);
        }
        card.collapsed = !card.collapsed;
        self.draw()?;
        Ok(true)
    }

    /// Folds or unfolds the newest card that actually has hidden rows -- the same card Ctrl+O
    /// toggles. Aiming at the literal last card made `/expand` target the note cell holding the
    /// command's own output rather than the tool output the user meant.
    fn set_last_collapsed(&mut self, collapsed: bool) -> Result<bool> {
        let Some(card) = self.last_foldable_mut() else {
            return Ok(false);
        };
        card.collapsed = collapsed;
        self.draw()?;
        Ok(true)
    }

    fn last_foldable_mut(&mut self) -> Option<&mut Card> {
        let index = last_foldable_index(&self.model.cards)?;
        self.model.cards.get_mut(index)
    }

    /// Appends to the note cell that is already open, or starts one. Consecutive lines from a
    /// single command stay in one cell; anything else pushed in between ends it naturally.
    fn add_notice(&mut self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        match self.model.cards.last_mut() {
            Some(card) if matches!(card.kind, CardKind::Note) => {
                if !card.body.is_empty() {
                    card.body.push('\n');
                }
                card.body.push_str(&text);
            }
            _ => {
                let id = self.take_card_id();
                self.model.cards.push(Card {
                    id,
                    kind: CardKind::Note,
                    title: String::new(),
                    body: text,
                    status: None,
                    collapsed: false,
                });
            }
        }
        self.trim_history();
        self.draw()
    }

    /// Opens a fresh cell headed by the command the user ran, so its output is attributed
    /// instead of merging into whatever was printed before it.
    fn add_command(&mut self, command: String) -> Result<()> {
        let id = self.take_card_id();
        self.model.cards.push(Card {
            id,
            kind: CardKind::Note,
            title: command,
            body: String::new(),
            status: None,
            collapsed: false,
        });
        self.trim_history();
        self.draw()
    }

    fn add_warning(&mut self, text: String) -> Result<()> {
        self.model.warnings.push(text);
        if self.model.warnings.len() > 32 {
            self.model.warnings.remove(0);
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

impl FullScreen {
    /// Hands the real terminal back: leaves the alternate screen, drops raw mode and mouse
    /// capture. Idempotent, because `Drop` also calls it for the paths that never shut down
    /// explicitly.
    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            SetCursorStyle::DefaultUserShape,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
        let _ = self.terminal.backend_mut().flush();
    }
}

impl Drop for FullScreen {
    fn drop(&mut self) {
        self.restore();
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
            let mut interval = tokio::time::interval(Duration::from_millis(50));
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

    /// A message folded into a turn that was already running. Rendered as an ordinary user
    /// message otherwise, and on re-reading a session there was no way to tell which of two
    /// prompts was the one that started the work.
    pub fn user_steering(&mut self, text: &str) -> Result<()> {
        self.card(
            CardKind::User,
            "steering \u{2192} added to the running turn",
            text,
        )
    }

    /// Echoes a slash command as its own transcript cell; its output lands in the same cell.
    pub fn command_echo(&mut self, command: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => lock_screen(screen).add_command(command.to_string()),
            None => Ok(()),
        }
    }

    pub fn tool_call(&mut self, name: &str, args: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                lock_screen(screen).add_card(CardKind::Tool, tool_header(name, args), args)
            }
            None => {
                print!(
                    "{}",
                    crate::render::render_tool_call(name, args, self.plain)
                );
                Ok(())
            }
        }
    }

    /// Reports a finished tool: `outcome` is the one-line result summary and always stays
    /// visible, `text` is the output hidden behind the fold. In plain (non-TUI) mode both are
    /// printed, matching the previous line-oriented output.
    pub fn tool_finished(&mut self, outcome: &str, ok: bool, text: &str) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                lock_screen(screen).finish_tool(outcome.to_string(), ok, text.to_string())
            }
            None => {
                if !text.is_empty() {
                    if ok {
                        print!("{}", crate::render::render_tool_output(text, self.plain));
                    } else {
                        print!("{}", crate::render::render_error(text, self.plain));
                    }
                }
                println!("{outcome}");
                io::stdout().flush().ok();
                Ok(())
            }
        }
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

    /// Appends a past answer as its own cell. Replay must not go through `assistant_update`:
    /// that one *replaces* the last assistant card, because it exists to grow a card as tokens
    /// stream in. Two stored answers in a row would overwrite each other.
    pub fn assistant_replay(&mut self, text: &str) -> Result<()> {
        self.card(CardKind::Output, "Assistant", text)
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
            Some(screen) => lock_screen(screen).add_warning(text.to_string()),
            None => {
                print!("{}", crate::render::render_warning(text, self.plain));
                io::stdout().flush()?;
                Ok(())
            }
        }
    }

    /// Show or hide warning messages in the transcript (`/warnings`).
    /// Toggles the keybinding sheet overlay (`?` and `/help` in TUI mode).
    pub fn toggle_help(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen_arc) => {
                let mut s = lock_screen(screen_arc);
                s.model.help_visible = !s.model.help_visible;
                s.draw()
            }
            None => Ok(()),
        }
    }

    /// Leaves the logo home screen (any command output means real UI begins).
    pub fn leave_intro(&mut self) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen_arc) => {
                let mut s = lock_screen(screen_arc);
                s.model.intro = false;
                s.draw()
            }
            None => Ok(()),
        }
    }

    /// Status-bar badge: text plus context percent (amber at/above 80%).
    pub fn set_token_badge(&mut self, badge: Option<(String, u8)>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.token_badge = badge;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    /// Per-warning detail lines (paths + reasons).
    /// Replaces the warning banner wholesale. `/warnings fix` re-checks the skills after the
    /// repair turn, and a banner that still lists folders which now load is worse than none.
    pub fn set_warnings(&mut self, lines: Vec<String>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warnings = lines;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warning_details(&mut self, details: Vec<String>) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warning_details = details;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warnings_expanded(&mut self, expanded: bool) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warning_details_visible = expanded;
                screen.draw()
            }
            None => Ok(()),
        }
    }

    pub fn set_warnings_visible(&mut self, visible: bool) -> Result<()> {
        match self.fullscreen.as_ref() {
            Some(screen) => {
                let mut screen = lock_screen(screen);
                screen.model.warnings_visible = visible;
                screen.draw()
            }
            None => Ok(()),
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
                    CardKind::Tool => crate::render::render_tool_call(
                        title.strip_prefix("Tool: ").unwrap_or(&title),
                        &body,
                        self.plain,
                    ),
                    CardKind::Output => crate::render::render_tool_output(&body, self.plain),
                    CardKind::Error => crate::render::render_error(&body, self.plain),
                    // Plain mode has no cells; a note is just a line of output.
                    CardKind::Note => format!(
                        "{body}
"
                    ),
                };
                print!("{rendered}");
                Ok(())
            }
        }
    }

    /// Leaves fullscreen and restores the real terminal. Anything printed after this lands in
    /// the user's scrollback; anything printed *before* it goes to the alternate screen, which
    /// is discarded on the way out. The exit summary has to come after.
    ///
    /// The terminal is restored in place rather than by dropping the handle: the input thread
    /// holds its own clone, so `Drop` would not run until that thread also lets go.
    pub fn leave_fullscreen(&mut self) {
        if let Some(screen) = self.fullscreen.take() {
            lock_screen(&screen).restore();
        }
    }

    /// Shareable key into the fullscreen terminal so a blocking input thread can render the
    /// live editor while the async chat loop awaits.
    pub fn screen_handle(&self) -> Option<ScreenHandle> {
        self.fullscreen.clone().map(ScreenHandle)
    }
}

// ---------------------------------------------------------------- input hub -----------------

/// Events the chat loop consumes from the keyboard hub.
pub enum HubEvent {
    /// A submitted line at an idle prompt.
    Line(String),
    /// Ctrl+C at an idle prompt — shut down.
    Quit,
}

/// Owns the keyboard for the whole session, opencode-style. While the agent runs the editor
/// stays live: typed lines queue instead of racing the turn, Esc raises an interrupt, and
/// page keys keep scrolling. Approval / ask_user prompts register a one-shot requester whose
/// answer bypasses the queue.
pub struct InputHub {
    rx: tokio::sync::mpsc::UnboundedReceiver<HubEvent>,
    pub interrupt: Arc<tokio::sync::Notify>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    queue: Arc<Mutex<VecDeque<String>>>,
    requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    candidates: Arc<std::sync::RwLock<Vec<crate::tui::Candidate>>>,
    screen: ScreenHandle,
    models_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    sessions_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
}

impl InputHub {
    /// Spawns the persistent keyboard thread. Call once at startup in TUI mode.
    pub fn spawn(screen: ScreenHandle) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub_screen = screen.clone();
        let interrupt = Arc::new(tokio::sync::Notify::new());
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>> =
            Arc::new(Mutex::new(None));
        let candidates = Arc::new(std::sync::RwLock::new(Vec::new()));
        let models_src = Arc::new(std::sync::RwLock::new(Vec::new()));
        let sessions_src = Arc::new(std::sync::RwLock::new(Vec::new()));
        std::thread::spawn({
            let interrupt = interrupt.clone();
            let busy = busy.clone();
            let queue = queue.clone();
            let requester = requester.clone();
            let candidates = candidates.clone();
            let models_src = models_src.clone();
            let sessions_src = sessions_src.clone();
            move || {
                input_thread(
                    screen,
                    tx,
                    interrupt,
                    busy,
                    queue,
                    requester,
                    candidates,
                    models_src,
                    sessions_src,
                )
            }
        });
        Self {
            rx,
            interrupt,
            busy,
            queue,
            requester,
            candidates,
            screen: hub_screen,
            models_src,
            sessions_src,
        }
    }

    /// Refreshes the slash-menu snapshot (commands/skills change rarely).
    pub fn set_candidates(&self, candidates: Vec<crate::tui::Candidate>) {
        *self
            .candidates
            .write()
            .unwrap_or_else(PoisonError::into_inner) = candidates;
    }

    /// Sources for the Ctrl+K model picker: (submit value, display label).
    pub fn set_models(&self, items: Vec<(String, String)>) {
        *self
            .models_src
            .write()
            .unwrap_or_else(PoisonError::into_inner) = items;
    }

    /// Opens the session switcher (Ctrl+S path shares this).
    pub fn open_sessions_dialog(&self) -> bool {
        let items = self
            .sessions_src
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Resume Session", "/resume ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    /// Generic modal picker: Enter submits prefix+value as a chat line.
    pub fn open_dialog(&self, title: &str, prefix: &str, items: Vec<(String, String)>) -> bool {
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new(title, prefix, items));
        }
        let _ = self.screen.draw_now();
        true
    }

    /// Opens the skills picker. It goes through the same dialog machinery as the model and
    /// session pickers on purpose: the previous popup drove `console::Term` directly, reading
    /// keys from the same stdin this hub's thread is already blocked on and painting raw ANSI
    /// into the alternate screen ratatui owns and repaints.
    pub fn open_skills_dialog(&self, items: Vec<(String, String)>) -> bool {
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Skills", "/skills toggle ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    pub fn open_models_dialog(&self) -> bool {
        let items = self
            .models_src
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if items.is_empty() {
            return false;
        }
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.dialog = Some(DialogState::new("Select Model", "/model ", items));
        }
        let _ = self.screen.draw_now();
        true
    }

    /// Sources for the Ctrl+S session switcher.
    pub fn set_sessions(&self, items: Vec<(String, String)>) {
        *self
            .sessions_src
            .write()
            .unwrap_or_else(PoisonError::into_inner) = items;
    }

    /// Marks the agent busy while the guard lives; Drop returns the prompt to idle.
    pub fn busy_guard(&self) -> BusyGuard {
        self.busy.store(true, std::sync::atomic::Ordering::SeqCst);
        BusyGuard {
            busy: self.busy.clone(),
        }
    }

    /// Next event from the keyboard.
    pub async fn next(&mut self) -> Option<HubEvent> {
        self.rx.recv().await
    }

    /// Enqueues a prompt programmatically (e.g. `/warnings fix`). Runs on the next idle pass.
    pub fn push_prompt(&self, line: String) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(line);
    }

    /// Pops a queued line, if any; drained between turns. Updates the footer count.
    pub fn pop_queue(&self) -> Option<String> {
        let popped = self
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        if popped.is_some() {
            let mut s = lock_screen(&self.screen.0);
            s.model.queued_count = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len();
            drop(s);
            let _ = self.screen.draw_now();
        }
        popped
    }

    /// Waits for a one-line answer (approval prompts, ask_user). Esc answers `None`.
    pub async fn request_line(&mut self) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self
            .requester
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx.await.ok()
    }

    /// Opens/closes the approval modal from the keyboard-thread side.
    pub fn open_permission_modal(&self, title: &str, body: String) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.permission = Some(PermissionState {
                title: title.to_string(),
                body,
                selected: 0,
                scroll: 0,
            });
        }
        let _ = self.screen.draw_now();
    }

    pub fn close_permission_modal(&self) {
        {
            let mut s = lock_screen(&self.screen.0);
            s.model.permission = None;
        }
        let _ = self.screen.draw_now();
    }
}

/// RAII marker telling the keyboard thread the agent is running.
pub struct BusyGuard {
    busy: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.busy.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Viewport height from the last draw; one page for PageUp/PageDown.
/// Suffix of `input` that fits `max` display columns - the editor scrolls horizontally,
/// keeping the caret (always at the end of the buffer) visible.
/// Byte index of the char boundary before `caret`, or 0.
fn prev_char_boundary(buf: &str, caret: usize) -> usize {
    buf[..caret.min(buf.len())]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// Byte index of the char boundary after `caret`, or the end.
fn next_char_boundary(buf: &str, caret: usize) -> usize {
    let caret = caret.min(buf.len());
    buf[caret..]
        .chars()
        .next()
        .map(|ch| caret + ch.len_utf8())
        .unwrap_or(buf.len())
}

/// Start of the word before `caret`: skip any run of spaces, then the run of non-spaces. This
/// is both where Alt+Left lands and what Ctrl+W deletes.
fn prev_word_boundary(buf: &str, caret: usize) -> usize {
    let mut index = caret.min(buf.len());
    while index > 0 {
        let previous = prev_char_boundary(buf, index);
        if buf[previous..index].chars().all(char::is_whitespace) {
            index = previous;
        } else {
            break;
        }
    }
    while index > 0 {
        let previous = prev_char_boundary(buf, index);
        if buf[previous..index].chars().any(char::is_whitespace) {
            break;
        }
        index = previous;
    }
    index
}

/// End of the word after `caret`, mirroring `prev_word_boundary`.
fn next_word_boundary(buf: &str, caret: usize) -> usize {
    let mut index = caret.min(buf.len());
    while index < buf.len() {
        let next = next_char_boundary(buf, index);
        if buf[index..next].chars().all(char::is_whitespace) {
            index = next;
        } else {
            break;
        }
    }
    while index < buf.len() {
        let next = next_char_boundary(buf, index);
        if buf[index..next].chars().any(char::is_whitespace) {
            break;
        }
        index = next;
    }
    index
}

/// Start of the buffer line holding `caret` (Home).
fn line_start(buf: &str, caret: usize) -> usize {
    buf[..caret.min(buf.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// End of the buffer line holding `caret` (End).
fn line_end(buf: &str, caret: usize) -> usize {
    let caret = caret.min(buf.len());
    buf[caret..]
        .find('\n')
        .map(|offset| caret + offset)
        .unwrap_or(buf.len())
}

fn page_rows(screen: &ScreenHandle) -> i64 {
    let s = lock_screen(&screen.0);
    s.last_viewport_rows.max(1) as i64
}

fn scroll_screen(screen: &ScreenHandle, rows: i64) {
    let mut s = lock_screen(&screen.0);
    let next = s.model.scroll_from_bottom as i64 + rows;
    s.model.scroll_from_bottom = next.clamp(0, 100_000) as usize;
    let _ = s.draw();
}

/// Centered modal with border, opencode PlaceOverlay style.
fn render_dialog(frame: &mut Frame<'_>, dialog: &DialogState, area: Rect) {
    let filtered = dialog.filtered();
    let selected = dialog.selected.min(filtered.len().saturating_sub(1));
    let width = 56.min(area.width.saturating_sub(4));
    let list_rows = filtered.len() as u16 + 2;
    let height = (list_rows + 3).min(area.height.saturating_sub(2)).max(5);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, box_area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(dialog.query.clone(), Style::default().fg(TEXT)),
            Span::styled("  (Esc closes)".to_string(), Style::default().fg(BORDER)),
        ]),
        Line::from(""),
    ];
    if filtered.is_empty() {
        lines.push(Line::styled(
            "(no match)".to_string(),
            Style::default().fg(MUTED),
        ));
    }
    for (idx, (value, label)) in filtered.iter().enumerate() {
        if idx >= selected.saturating_sub(7) && idx < selected.saturating_sub(7) + 8 {
            let is_on = idx == selected;
            let prefix = if is_on { "\u{276f} " } else { "  " };
            let mut row = vec![
                Span::styled(
                    prefix.to_string(),
                    Style::default().fg(if is_on { BLUE } else { BORDER }),
                ),
                Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(if is_on { TEXT } else { MUTED })
                        .add_modifier(if is_on {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            // Show the raw value only when the label doesn't already contain it.
            if !label.contains(value.as_str()) {
                row.push(Span::raw("  "));
                row.push(Span::styled(value.clone(), Style::default().fg(BORDER)));
            }
            lines.push(Line::from(row));
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BLUE))
                    .title(format!(" {} ", dialog.title))
                    .title_style(Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
            ),
        box_area,
    );
}

/// Approval modal: preview body plus the three opencode options.
fn render_permission(frame: &mut Frame<'_>, perm: &PermissionState, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4));
    let all_rows: Vec<String> = wrap_display(&perm.body, width.saturating_sub(6) as usize);
    // Everything the box spends on chrome: blank row, options, the scroll note, the key hint.
    let chrome = PERM_OPTIONS.len() + 5;
    let ceiling = area.height.saturating_sub(2).max(7) as usize;
    let capacity = ceiling.saturating_sub(chrome).max(1);
    let scroll = perm.scroll.min(all_rows.len().saturating_sub(capacity));
    let body_rows: Vec<String> = all_rows
        .iter()
        .skip(scroll)
        .take(capacity)
        .cloned()
        .collect();
    let height = ((body_rows.len() + chrome) as u16)
        .min(area.height.saturating_sub(2))
        .max(7);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, box_area);
    let mut lines = Vec::new();
    for row in &body_rows {
        let trimmed = row.trim_start();
        let style = if trimmed.starts_with("- ") {
            Style::default().fg(RED)
        } else if trimmed.starts_with("+ ") {
            Style::default().fg(GREEN)
        } else if trimmed.starts_with("--- ") {
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD)
        } else if trimmed.starts_with("…") {
            Style::default().fg(MUTED).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::styled(row.clone(), style));
    }
    if all_rows.len() > body_rows.len() {
        lines.push(Line::styled(
            format!(
                "\u{2026} showing {}-{} of {} lines \u{b7} PgUp/PgDn",
                scroll + 1,
                scroll + body_rows.len(),
                all_rows.len()
            ),
            Style::default().fg(WARN),
        ));
    }
    lines.push(Line::from(""));
    for (idx, (_, label)) in PERM_OPTIONS.iter().enumerate() {
        let is_on = idx == perm.selected;
        let prefix = if is_on { "\u{276f} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE } else { BORDER }),
            ),
            Span::styled(
                (*label).to_string(),
                Style::default()
                    .fg(match (idx, is_on) {
                        (2, true) => RED,
                        (2, false) => MUTED,
                        (_, true) => TEXT,
                        _ => MUTED,
                    })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "Enter confirm \u{b7} Esc rejects".to_string(),
        Style::default().fg(BORDER),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(WARN))
                    .title(format!(" {} ", perm.title))
                    .title_style(Style::default().fg(WARN).add_modifier(Modifier::BOLD)),
            ),
        box_area,
    );
}

/// `?` overlay: the keybinding sheet.
fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = 26.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let box_area = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, box_area);
    let rows: [(&str, &str); 22] = [
        ("Enter", "send message"),
        ("Shift/Ctrl+Enter", "newline without sending"),
        ("\u{2190}/\u{2192}", "move the caret"),
        ("Alt+\u{2190}/\u{2192}", "move by word"),
        ("Home/End, Ctrl+A/E", "start / end of line"),
        ("Ctrl+K", "switch model"),
        ("Ctrl+S", "resume a session"),
        ("Ctrl+O / click", "expand or fold tool output"),
        ("Ctrl+F", "search the transcript"),
        ("Ctrl+B", "show or hide the sidebar"),
        ("Ctrl+Y", "copy the latest answer"),
        ("Right click", "copy the cell under the pointer"),
        ("?", "toggle this help"),
        ("Tab / Shift+Tab", "cycle mode (build / auto / plan)"),
        ("Tab", "accept slash completion, when the menu is open"),
        ("\u{2191}/\u{2193}", "history / menu navigation"),
        ("PgUp/PgDn", "scroll transcript"),
        ("Ctrl+Home/End", "jump to top/bottom"),
        ("!<command>", "run a shell command"),
        ("/warnings", "hide or show warnings"),
        ("Esc", "interrupt the agent"),
        ("Ctrl+C x 2", "quit"),
    ];
    let mut lines = vec![Line::from(Span::styled(
        "Keybindings",
        Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
    ))];
    for (key, desc) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<12} "),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(desc.to_string(), Style::default().fg(MUTED)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(POPUP_BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BORDER)),
            ),
        box_area,
    );
}

/// The single keyboard reader for TUI mode.
#[allow(clippy::too_many_arguments)]
fn input_thread(
    screen: ScreenHandle,
    tx: tokio::sync::mpsc::UnboundedSender<HubEvent>,
    interrupt: Arc<tokio::sync::Notify>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    queue: Arc<Mutex<VecDeque<String>>>,
    requester: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    candidates: Arc<std::sync::RwLock<Vec<crate::tui::Candidate>>>,
    models_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
    sessions_src: Arc<std::sync::RwLock<Vec<(String, String)>>>,
) {
    let mut buf = String::new();
    // Caret as a byte offset into `buf`. Editing used to be append-and-backspace only: a typo
    // near the start of a long prompt meant deleting everything back to it.
    let mut caret = 0usize;
    let mut selected = 0usize;
    let mut history: Vec<String> = Vec::new();
    let mut history_idx = 0usize;
    let mut saved_buf = String::new();
    let mut last_ctrl_c: Option<std::time::Instant> = None;

    let sync = |screen: &ScreenHandle,
                buf: &str,
                caret: usize,
                selected: usize,
                items: Vec<(String, String)>| {
        let mut s = lock_screen(&screen.0);
        s.model.input = buf.to_string();
        s.model.input_caret = caret.min(buf.len());
        s.model.ac_selected = selected;
        s.model.ac_items = items;
        let _ = s.draw();
    };
    let items_for = |needle: &str| -> Vec<(String, String)> {
        candidates
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|c| c.name.to_ascii_lowercase().starts_with(needle))
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    };

    sync(&screen, "", 0, 0, Vec::new());
    // Feed loop: the wheel scrolls right here; only key presses fall through to the editor.
    // Read errors are tolerated briefly, then quit gracefully (never process::exit - that
    // would skip FullScreen's Drop and leave raw mode + mouse capture enabled).
    let mut feed_errors = 0u32;
    'keys: loop {
        let key = 'feed: {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => break 'feed Some(key),
                // A paste is one event carrying its whole payload, so newlines inside it stay
                // in the buffer instead of each submitting a separate message.
                Ok(Event::Paste(pasted)) => {
                    let cleaned = pasted.replace("\r\n", "\n").replace('\r', "\n");
                    if !cleaned.is_empty() {
                        buf.insert_str(caret, &cleaned);
                        caret += cleaned.len();
                        selected = 0;
                        if history_idx == history.len() {
                            saved_buf = buf.clone();
                        }
                        let is_slash =
                            buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
                        let items = if is_slash {
                            items_for(&needle_of(&buf))
                        } else {
                            Vec::new()
                        };
                        sync(&screen, &buf, caret, selected, items);
                    }
                }
                Ok(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => scroll_screen(&screen, 3),
                    MouseEventKind::ScrollDown => scroll_screen(&screen, -3),
                    // Click-to-expand: the row map from the last draw resolves the pointer to
                    // a card id, so folds open without leaving the mouse.
                    MouseEventKind::Down(MouseButton::Left) => {
                        let _ = lock_screen(&screen.0).toggle_card_at_row(mouse.row);
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        let _ = lock_screen(&screen.0).copy_card_at_row(mouse.row);
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => {
                    feed_errors += 1;
                    if feed_errors >= 5 {
                        let _ = tx.send(HubEvent::Quit);
                        break 'feed None;
                    }
                }
            }
            continue 'keys;
        };
        let Some(key) = key else { break };
        feed_errors = 0;

        // --- Approval modal owns the keys while open ---
        {
            let mut s = lock_screen(&screen.0);
            if let Some(perm) = s.model.permission.as_mut() {
                match key.code {
                    KeyCode::Up => {
                        perm.selected = perm
                            .selected
                            .checked_sub(1)
                            .unwrap_or(PERM_OPTIONS.len() - 1);
                    }
                    KeyCode::Down => {
                        perm.selected = (perm.selected + 1) % PERM_OPTIONS.len();
                    }
                    // Up/Down belong to the options, so the body pages instead.
                    KeyCode::PageUp => {
                        perm.scroll = perm.scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        perm.scroll += 5;
                    }
                    KeyCode::Enter => {
                        let answer = PERM_OPTIONS[perm.selected.min(PERM_OPTIONS.len() - 1)]
                            .0
                            .to_string();
                        s.model.permission = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send(answer);
                        }
                        continue;
                    }
                    KeyCode::Esc => {
                        s.model.permission = None;
                        drop(s);
                        if let Some(tx) = requester
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                        {
                            let _ = tx.send("n".to_string());
                        }
                        continue;
                    }
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            drop(s);
        }

        // --- Modal overlays own the keys while open (opencode dialogs) ---
        {
            let mut s = lock_screen(&screen.0);
            if s.model.help_visible {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Esc => s.model.help_visible = false,
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            if let Some(dialog) = s.model.dialog.as_mut() {
                match key.code {
                    KeyCode::Up => {
                        let total = dialog.filtered().len();
                        if total > 0 {
                            dialog.selected = dialog.selected.checked_sub(1).unwrap_or(total - 1);
                        }
                    }
                    KeyCode::Down => {
                        let total = dialog.filtered().len();
                        if total > 0 {
                            dialog.selected = (dialog.selected + 1) % total;
                        }
                    }
                    KeyCode::Enter => {
                        let picked = dialog
                            .filtered()
                            .get(dialog.selected)
                            .map(|(value, _)| (*value).clone());
                        if let Some(value) = picked {
                            let line = format!("{}{}", dialog.prefix, value);
                            s.model.dialog = None;
                            drop(s);
                            submit_line(&screen, &tx, &requester, &busy, &queue, line);
                            continue;
                        }
                    }
                    KeyCode::Esc => s.model.dialog = None,
                    KeyCode::Backspace => {
                        dialog.query.pop();
                        dialog.selected = 0;
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        dialog.query.push(c);
                        dialog.selected = 0;
                    }
                    _ => {}
                }
                drop(s);
                let _ = screen.draw_now();
                continue;
            }
            drop(s);
        }

        // Transcript search owns the keyboard while it is open, so its query cannot leak into
        // the editor buffer.
        let searching = lock_screen(&screen.0).model.search.is_some();
        if searching {
            let mut sc = lock_screen(&screen.0);
            match key.code {
                KeyCode::Esc => {
                    let _ = sc.close_search();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = sc.close_search();
                }
                KeyCode::Enter | KeyCode::Down => {
                    let _ = sc.step_search(1);
                }
                KeyCode::Up => {
                    let _ = sc.step_search(-1);
                }
                KeyCode::Backspace => {
                    let _ = sc.edit_search(|query| {
                        query.pop();
                    });
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = sc.edit_search(|query| query.push(c));
                }
                _ => {}
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            let _ = lock_screen(&screen.0).open_search();
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            let mut sc = lock_screen(&screen.0);
            sc.model.sidebar_hidden = !sc.model.sidebar_hidden;
            let _ = sc.draw();
            continue;
        }

        // Openers.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            let items = models_src
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if !items.is_empty() {
                let mut sc = lock_screen(&screen.0);
                sc.model.dialog = Some(DialogState::new("Select Model", "/model ", items));
                drop(sc);
                let _ = screen.draw_now();
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            let items = sessions_src
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if !items.is_empty() {
                let mut sc = lock_screen(&screen.0);
                sc.model.dialog = Some(DialogState::new("Resume Session", "/resume ", items));
                drop(sc);
                let _ = screen.draw_now();
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            let _ = lock_screen(&screen.0).toggle_last_card();
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            let _ = lock_screen(&screen.0).copy_latest();
            continue;
        }
        if key.code == KeyCode::Char('?') && buf.is_empty() {
            let mut sc = lock_screen(&screen.0);
            sc.model.help_visible = !sc.model.help_visible;
            drop(sc);
            let _ = screen.draw_now();
            continue;
        }

        let is_slash = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let needle = buf
            .trim_start()
            .trim_start_matches('/')
            .to_ascii_lowercase();
        let is_busy = busy.load(std::sync::atomic::Ordering::SeqCst);
        if !matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
        {
            last_ctrl_c = None;
        }
        match key.code {
            KeyCode::Up => {
                if is_slash {
                    let total = filtered_len(&candidates, &needle);
                    if total > 0 {
                        selected = selected.checked_sub(1).unwrap_or(total - 1);
                    }
                } else if history_idx > 0 {
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                    history_idx -= 1;
                    buf = history[history_idx].clone();
                    caret = buf.len();
                    selected = 0;
                }
            }
            KeyCode::Down => {
                if is_slash {
                    let total = filtered_len(&candidates, &needle);
                    if total > 0 {
                        selected = (selected + 1) % total;
                    }
                } else if history_idx < history.len() {
                    history_idx += 1;
                    buf = if history_idx == history.len() {
                        saved_buf.clone()
                    } else {
                        history[history_idx].clone()
                    };
                    caret = buf.len();
                    selected = 0;
                }
            }
            KeyCode::Tab => {
                if is_slash {
                    let all = items_for(&needle);
                    if let Some(choice) = all.get(selected) {
                        buf = format!("/{} ", choice.0);
                        caret = buf.len();
                        selected = 0;
                    }
                } else {
                    // opencode cycles agents with Tab. Completion keeps first claim on the key
                    // while the slash menu is open, so the two never compete for it.
                    submit_line(
                        &screen,
                        &tx,
                        &requester,
                        &busy,
                        &queue,
                        "/mode next".to_string(),
                    );
                }
            }
            KeyCode::BackTab => {
                submit_line(
                    &screen,
                    &tx,
                    &requester,
                    &busy,
                    &queue,
                    "/mode prev".to_string(),
                );
            }
            // Caret motion. Alt jumps by word, plain arrows by character.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                caret = prev_word_boundary(&buf, caret);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                caret = next_word_boundary(&buf, caret);
            }
            KeyCode::Left => caret = prev_char_boundary(&buf, caret),
            KeyCode::Right => caret = next_char_boundary(&buf, caret),
            KeyCode::Delete => {
                let end = next_char_boundary(&buf, caret);
                if end > caret {
                    buf.replace_range(caret..end, "");
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            // Ctrl+W deletes the word before the caret, as in a shell.
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let start = prev_word_boundary(&buf, caret);
                if start < caret {
                    buf.replace_range(start..caret, "");
                    caret = start;
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                caret = line_start(&buf, caret);
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                caret = line_end(&buf, caret);
            }
            // Newline without submitting. opencode binds shift+return / ctrl+return /
            // alt+return / ctrl+j for this; terminals disagree about which of those they
            // report, so accept all of them. The trailing-\\ form still works.
            KeyCode::Enter
                if key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ) =>
            {
                buf.insert(caret, '\n');
                caret += 1;
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.insert(caret, '\n');
                caret += 1;
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            KeyCode::Enter => {
                // opencode editor behavior: a backslash before the caret escapes the newline
                // and continues the message on the next line. Read at the caret rather than at
                // the end of the buffer, so it still works mid-line.
                let before = prev_char_boundary(&buf, caret);
                if before < caret && &buf[before..caret] == "\\" {
                    buf.replace_range(before..caret, "\n");
                    caret = before + 1;
                    selected = 0;
                    // Sync before looping: returning early used to leave the new line undrawn
                    // until the next keystroke.
                    sync(&screen, &buf, caret, selected, Vec::new());
                    continue 'keys;
                }
                let line = buf.trim().to_string();
                buf.clear();
                caret = 0;
                selected = 0;
                if !line.is_empty() {
                    history.push(line.clone());
                    if history.len() > 500 {
                        history.remove(0);
                    }
                    history_idx = history.len();
                }
                submit_line(&screen, &tx, &requester, &busy, &queue, line);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, -(page_rows(&screen) / 2));
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, page_rows(&screen) / 2);
            }
            KeyCode::Esc => {
                if is_busy {
                    interrupt.notify_one();
                    buf.clear();
                    caret = 0;
                    selected = 0;
                    let _ = lock_screen(&screen.0).add_notice("interrupt requested");
                } else {
                    buf.clear();
                    caret = 0;
                    selected = 0;
                }
            }
            KeyCode::PageUp => scroll_screen(&screen, page_rows(&screen)),
            KeyCode::PageDown => scroll_screen(&screen, -page_rows(&screen)),
            // Home/End move the caret, which is what they do in every other text field.
            // Jumping the transcript to top/bottom moved to Ctrl+Home / Ctrl+End.
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, 100_000);
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                scroll_screen(&screen, -100_000);
            }
            KeyCode::Home => caret = line_start(&buf, caret),
            KeyCode::End => caret = line_end(&buf, caret),
            KeyCode::Backspace => {
                let start = prev_char_boundary(&buf, caret);
                if start < caret {
                    buf.replace_range(start..caret, "");
                    caret = start;
                    selected = 0;
                    if history_idx == history.len() {
                        saved_buf = buf.clone();
                    }
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if is_busy {
                    interrupt.notify_one();
                } else if last_ctrl_c
                    .map(|t| t.elapsed() < std::time::Duration::from_secs(3))
                    .unwrap_or(false)
                {
                    let _ = tx.send(HubEvent::Quit);
                    return;
                } else {
                    last_ctrl_c = Some(std::time::Instant::now());
                    let _ =
                        lock_screen(&screen.0).add_notice("Press Ctrl+C again within 3s to quit.");
                }
            }
            KeyCode::Char(c) => {
                buf.insert(caret, c);
                caret += c.len_utf8();
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
                }
            }
            _ => {}
        }
        // Menu visibility follows the post-keystroke buffer: an empty or non-slash line
        // closes it (filtering with an empty needle would otherwise match every candidate
        // and leave the helper stuck open).
        let is_slash_now = buf.trim_start().starts_with('/') && !buf.trim_start().contains(' ');
        let (items, selected) = if is_slash_now {
            (items_for(&needle_of(&buf)), selected)
        } else {
            (Vec::new(), 0)
        };
        sync(&screen, &buf, caret, selected, items);
    }
}

/// Shared submit path for the editor and modal dialogs: a waiting approval/ask_user takes
/// the answer, busy queues it, idle sends it straight to the chat loop.
fn submit_line(
    screen: &ScreenHandle,
    tx: &tokio::sync::mpsc::UnboundedSender<HubEvent>,
    requester: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    busy: &Arc<std::sync::atomic::AtomicBool>,
    queue: &Arc<Mutex<VecDeque<String>>>,
    line: String,
) {
    if line.is_empty() {
        return;
    }
    let answer_tx = requester
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(tx) = answer_tx {
        let _ = tx.send(line);
    } else if busy.load(std::sync::atomic::Ordering::SeqCst) {
        queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(line.clone());
        let mut s = lock_screen(&screen.0);
        s.model.queued_count = queue.lock().unwrap_or_else(PoisonError::into_inner).len();
        let _ = s.add_notice(format!("queued: {line}"));
        drop(s);
    } else {
        let _ = tx.send(HubEvent::Line(line));
    }
}

fn needle_of(buf: &str) -> String {
    buf.trim_start()
        .trim_start_matches('/')
        .to_ascii_lowercase()
}

fn filtered_len(candidates: &std::sync::RwLock<Vec<crate::tui::Candidate>>, needle: &str) -> usize {
    crate::tui::filter_candidates(
        &candidates.read().unwrap_or_else(PoisonError::into_inner),
        needle,
    )
    .len()
}

/// Blocking editor loop, opencode-style: keystrokes update `Model::input` and redraw through
/// ratatui, so typed text lives inside the bordered editor instead of a raw ANSI popup below
/// the frame. Runs on a dedicated thread (see `spawn_blocking` callers).
#[derive(Clone)]
pub struct ScreenHandle(Arc<Mutex<FullScreen>>);

impl ScreenHandle {
    /// Redraws the frame from current model state.
    pub fn draw_now(&self) -> Result<()> {
        lock_screen(&self.0).draw()
    }
}

/// What one draw learned about the frame: the transcript viewport height (PageUp/PageDown
/// page by it) and which card owns each drawn row (a click resolves through it).
#[derive(Default)]
struct RenderInfo {
    viewport_rows: usize,
    card_rows: Vec<(u16, u64)>,
    transcript_width: u16,
}

fn render(frame: &mut Frame<'_>, model: &Model) -> RenderInfo {
    // OpenCode layout: transcript on top, autocomplete menu above the bordered editor, a
    // one-line footer, and the sidebar rail splitting the body horizontally.
    // The search bar and the slash menu never coexist: opening search closes the editor's menu.
    let popup_height = if model.search.is_some() {
        1
    } else {
        menu_height(model.ac_items.len())
    };
    // Multiline editor: grows with the buffer's newlines (backslash-newline continuation).
    // Split the same way `editor_widget` does: `lines()` drops a trailing empty segment, which
    // would leave the caret a row below the text after the buffer ends with a newline.
    let input_lines = model.input.split('\n').count().max(1);
    let editor_rows =
        (input_lines.min(EDITOR_VISIBLE_LINES) as u16) + 2 + u16::from(model.thinking.is_some());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(popup_height),
            Constraint::Length(editor_rows),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let body_area = rows[0];
    let popup_area = rows[1];
    let editor_area = rows[2];
    let footer_area = rows[3];

    // Sidebar rail takes the right side once the terminal is wide enough and chat is underway.
    let (transcript_area, sidebar_area) = match (&model.sidebar, model.intro) {
        (Some(entries), false)
            if !entries.is_empty() && !model.sidebar_hidden && body_area.width >= 68 =>
        {
            // Narrow terminals get a narrower rail rather than none at all.
            let rail = if body_area.width >= 84 { 30 } else { 24 };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(rail)])
                .split(body_area);
            (cols[0], Some(cols[1]))
        }
        _ => (body_area, None),
    };

    let mut card_rows: Vec<(u16, u64)> = Vec::new();
    let home = model.intro
        && model
            .cards
            .iter()
            .all(|card| matches!(card.kind, CardKind::Note));
    if home {
        frame.render_widget(intro_paragraph(model, body_area), body_area);
    } else {
        // Wrap every row ourselves so viewport math is exact (Paragraph's own wrap happens
        // after scroll offsets, which makes bottom-follow drift on long wrapped lines).
        // Each source line carries its owning card, so wrapping cannot lose the attribution
        // a click needs.
        let rows = wrapped_transcript(model, transcript_area.width);
        let visible = transcript_area.height as usize;
        let start = rows
            .len()
            .saturating_sub(visible.saturating_add(model.scroll_from_bottom));
        let hits = model
            .search
            .as_ref()
            .map(|search| matching_rows(&rows, &search.query))
            .unwrap_or_default();
        let current_hit = model
            .search
            .as_ref()
            .filter(|_| !hits.is_empty())
            .map(|search| hits[search.current % hits.len()]);
        let mut window: Vec<Line<'static>> = Vec::with_capacity(visible);
        for (offset, (line, owner)) in rows.into_iter().skip(start).take(visible).enumerate() {
            if let Some(id) = owner {
                card_rows.push((transcript_area.y + offset as u16, id));
            }
            let absolute = start + offset;
            let line = if Some(absolute) == current_hit {
                highlight_row(line, MATCH_CURRENT_BG)
            } else if hits.binary_search(&absolute).is_ok() {
                highlight_row(line, MATCH_BG)
            } else {
                line
            };
            window.push(line);
        }
        frame.render_widget(Paragraph::new(Text::from(window)), transcript_area);
    }
    if let Some(area) = sidebar_area {
        frame.render_widget(sidebar_paragraph(model, area), area);
    }
    if let Some(search) = &model.search {
        frame.render_widget(search_widget(search), popup_area);
    } else if popup_height > 0 {
        frame.render_widget(popup_widget(model), popup_area);
    }
    frame.render_widget(editor_widget(model, editor_area), editor_area);

    // Terminal cursor sits at the end of the typed text whenever the editor owns input. This
    // includes the home screen: ratatui hides the cursor on any frame that sets no position,
    // so skipping it there left the first thing a user types with no visible caret.
    {
        let inner = editor_area.width.saturating_sub(4).max(1) as usize;
        let view = editor_view(&model.input, model.input_caret, inner);
        // The editor block draws a LEFT border: one column, no rows. Text then starts after the
        // two-cell row prefix, so the caret belongs at x + 3 on the block's own first row --
        // an earlier `y + 1` aimed at a top border that this block never draws.
        let row = editor_area.y + view.caret_row as u16;
        let col = editor_area.x + 3 + view.caret_col.min(inner) as u16;
        frame.set_cursor_position((
            col.min(editor_area.right().saturating_sub(1)),
            row.min(editor_area.bottom().saturating_sub(1)),
        ));
    }

    frame.render_widget(footer_widget(model), footer_area);

    if let Some(perm) = &model.permission {
        render_permission(frame, perm, frame.area());
    }
    if model.help_visible {
        render_help(frame, frame.area());
    }
    if let Some(dialog) = &model.dialog {
        render_dialog(frame, dialog, frame.area());
    }

    RenderInfo {
        viewport_rows: if home { 0 } else { body_area.height as usize },
        card_rows,
        transcript_width: transcript_area.width,
    }
}

/// The newest card with rows hidden behind a fold -- the one Ctrl+O, `/expand`, and
/// `/collapse` all act on. Command output is a cell too now, so "the last card" is usually the
/// note holding that command's own output rather than the tool output worth unfolding.
fn last_foldable_index(cards: &[Card]) -> Option<usize> {
    cards.iter().rposition(|card| card.foldable_rows() > 0)
}

/// Puts text on the system clipboard. Kept behind one function so the failure mode -- a
/// headless session with no clipboard at all -- is reported once, as a notice, instead of
/// taking the UI down.
fn set_clipboard_text(text: &str) -> Result<()> {
    arboard::Clipboard::new()
        .context("could not access the system clipboard")?
        .set_text(text.to_string())
        .context("could not write to the system clipboard")
}

/// The editor's visible rows together with where the caret sits among them. Rows and caret come
/// from one function on purpose: derived separately they drift apart, which is how the caret
/// ended up pointing at a row the text was never drawn on.
struct EditorView {
    rows: Vec<String>,
    caret_row: usize,
    /// Display columns from the start of the row's text.
    caret_col: usize,
}

/// Lays out `input` for an editor `width` columns wide, scrolled so the caret is always on
/// screen both vertically (long multi-line buffers) and horizontally (long single lines).
fn editor_view(input: &str, caret: usize, width: usize) -> EditorView {
    let width = width.max(1);
    let caret = caret.min(input.len());
    let segments: Vec<&str> = input.split('\n').collect();

    // Which buffer line the caret is on, and how many chars into it.
    let start_of_line = line_start(input, caret);
    let caret_row_full = input[..start_of_line].matches('\n').count();
    let caret_chars = input[start_of_line..caret].chars().count();

    // Vertical viewport: the newest lines, extended back if the caret sits above them.
    let mut first = segments.len().saturating_sub(EDITOR_VISIBLE_LINES);
    first = first.min(caret_row_full);

    let mut rows = Vec::with_capacity(segments.len() - first);
    let mut caret_col = 0usize;
    for (offset, segment) in segments[first..].iter().enumerate() {
        if first + offset == caret_row_full {
            let (visible, column) = visible_around_caret(segment, caret_chars, width);
            caret_col = column;
            rows.push(visible);
        } else {
            rows.push(segment.chars().take(width).collect());
        }
    }
    EditorView {
        rows,
        caret_row: caret_row_full - first,
        caret_col,
    }
}

/// The slice of one buffer line that fits in `width` columns while keeping the caret visible,
/// plus the caret's column inside that slice.
fn visible_around_caret(segment: &str, caret_chars: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = segment.chars().collect();
    // One column is reserved so a caret at the very end of the line still has somewhere to sit.
    let span = width.saturating_sub(1).max(1);
    let start = caret_chars.saturating_sub(span);
    let end = chars.len().min(start + width);
    let visible: String = chars[start.min(chars.len())..end].iter().collect();
    let column = UnicodeWidthStr::width(
        chars[start.min(chars.len())..caret_chars.min(chars.len())]
            .iter()
            .collect::<String>()
            .as_str(),
    );
    (visible, column)
}

/// How many buffer rows the editor shows at once; longer buffers scroll to the newest.
const EDITOR_VISIBLE_LINES: usize = 5;

/// The opencode-style prompt: left accent border, element background, `❯` glyph with the live
/// buffer, and the caret sitting at the end of the buffer.
fn editor_widget(model: &Model, area: Rect) -> Paragraph<'static> {
    // Horizontal viewport: keep the caret (always at the end of the buffer) on screen.
    let mut rows: Vec<Line<'static>> = match () {
        _ if model.input.is_empty() => vec![Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if model.thinking.is_some() {
                    "type to steer \u{2014} your message joins the running turn".to_string()
                } else {
                    "type a message, or / for commands".to_string()
                },
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])],
        _ => {
            // One row per newline segment, scrolled to keep the caret in view. Joining the
            // segments into a single Line (as this did) collapsed a multi-line buffer onto one
            // row while the caret was placed per segment, so the two disagreed about the text.
            let inner = area.width.saturating_sub(4).max(1) as usize;
            let view = editor_view(&model.input, model.input_caret, inner);
            view.rows
                .into_iter()
                .enumerate()
                .map(|(offset, row)| {
                    Line::from(vec![
                        // Continuation rows repeat the prompt glyph's width so the text column
                        // -- and therefore the caret column -- is the same on every row.
                        if offset == 0 && model.input_caret <= model.input.len() {
                            Span::styled(
                                "\u{276f} ".to_string(),
                                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                            )
                        } else {
                            Span::raw("  ".to_string())
                        },
                        Span::styled(row, Style::default().fg(TEXT)),
                    ])
                })
                .collect()
        }
    };
    // Loading state: the editor keeps accepting input, so the run needs its own row here to
    // say that something is in flight and how to stop it. The input uses a bouncing wall
    // to avoid duplicating the response-area spinner (which stays as the spinner).
    if let Some((frame_idx, _label)) = model.thinking {
        let mut wall_line: Vec<Span<'static>> = vec![Span::raw("  ".to_string())];
        wall_line.extend(bouncing_wall_spans(frame_idx));
        wall_line.push(Span::raw(" ".to_string()));
        // pulse the label so the bar never looks frozen — wall moves, dots breathe
        let dots = ".".repeat(frame_idx % 4);
        wall_line.push(Span::styled(
            format!("processing{dots}"),
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ));
        wall_line.push(Span::styled(
            "  \u{b7} Enter steers \u{b7} Esc interrupts".to_string(),
            Style::default().fg(BORDER).add_modifier(Modifier::DIM),
        ));
        rows.push(Line::from(wall_line));
    }
    Paragraph::new(Text::from(rows))
        .style(Style::default().bg(BG_ELEMENT))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(BLUE)),
        )
}

/// Slash-command menu rendered above the editor while the buffer looks like a command.
/// How many menu rows fit above the editor at once; arrows scroll within this window.
const MENU_VISIBLE: usize = 8;

pub(crate) fn menu_height(item_count: usize) -> u16 {
    if item_count == 0 {
        0
    } else {
        (item_count.min(MENU_VISIBLE) as u16 + 2).min(10)
    }
}

/// The search bar: what was typed, and which match of how many is in view.
fn search_widget(search: &SearchState) -> Paragraph<'static> {
    let readout = if search.query.is_empty() {
        "type to search the transcript".to_string()
    } else if search.total == 0 {
        "no matches".to_string()
    } else {
        format!("{}/{}", search.current % search.total + 1, search.total)
    };
    Paragraph::new(Text::from(Line::from(vec![
        Span::styled(
            "search ".to_string(),
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(search.query.clone(), Style::default().fg(TEXT)),
        Span::styled(
            format!("  {readout}"),
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ),
        Span::styled(
            "  \u{b7} Enter next \u{b7} Up prev \u{b7} Esc close".to_string(),
            Style::default().fg(BORDER).add_modifier(Modifier::DIM),
        ),
    ])))
    .style(Style::default().bg(POPUP_BG))
}

fn popup_widget(model: &Model) -> Paragraph<'static> {
    let total = model.ac_items.len();
    let selected = model.ac_selected.min(total.saturating_sub(1));
    // Sliding window keeps the highlighted row in view no matter how many candidates match.
    let start = if total <= MENU_VISIBLE {
        0
    } else {
        selected
            .saturating_sub(MENU_VISIBLE / 2)
            .min(total - MENU_VISIBLE)
    };
    let end = total.min(start + MENU_VISIBLE);

    let counter = if total > MENU_VISIBLE {
        format!(" {} / {}", selected + 1, total)
    } else {
        String::new()
    };
    let mut lines: Vec<Line<'static>> = vec![Line::from(vec![
        Span::styled("\u{2500}".repeat(40), Style::default().fg(BORDER)),
        Span::styled(counter, Style::default().fg(BORDER)),
    ])];
    for idx in start..end {
        let (name, description) = &model.ac_items[idx];
        let is_on = idx == selected;
        let prefix = if is_on { "\u{276f} " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default().fg(if is_on { BLUE } else { BORDER }),
            ),
            Span::styled(
                crate::tui::truncate_chars(name, 24),
                Style::default()
                    .fg(if is_on { TEXT } else { MUTED })
                    .add_modifier(if is_on {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::raw("  "),
            Span::styled(
                crate::tui::truncate_chars(description, 48),
                Style::default().fg(MUTED),
            ),
        ]));
    }
    Paragraph::new(Text::from(lines)).style(Style::default().bg(POPUP_BG))
}

fn sidebar_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(entries) = &model.sidebar {
        for (key, value) in entries {
            lines.push(Line::from(Span::styled(
                format!("{key} "),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )));
            // Values may carry newlines (Last turn metrics); ratatui strips them inside
            // spans, so split before styling.
            for value_line in value.split('\n') {
                lines.push(Line::styled(
                    crate::tui::truncate_chars(value_line, area.width.saturating_sub(4) as usize),
                    Style::default().fg(NOTICE_FG),
                ));
            }
            lines.push(Line::from(""));
        }
    }
    Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(BORDER)),
    )
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
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
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
    // Startup notices still belong on the home screen — the logo must never hide them.
    for card in &model.cards {
        for row in card.title.lines().chain(card.body.lines()) {
            lines.push(Line::from(""));
            lines.push(Line::styled(row.to_string(), Style::default().fg(MUTED)));
        }
    }
    if model.warnings_visible {
        for warning in &model.warnings {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!("\u{26a0} {warning}"),
                Style::default().fg(WARN),
            ));
        }
        if model.warning_details_visible {
            for detail in &model.warning_details {
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    format!("  ↳ {detail}"),
                    Style::default().fg(MUTED),
                ));
            }
        }
    }
    Paragraph::new(Text::from(lines))
}

/// One quiet status line — the editor above it carries the prompt itself. Queue depth and
/// interrupt hints appear while the agent is working.
fn footer_widget(model: &Model) -> Paragraph<'static> {
    let mut text = model.footer.clone();
    if model.thinking.is_some() {
        text.push_str(" · Esc interrupts");
    }
    if model.scroll_from_bottom > 0 {
        text.push_str(&format!(
            "  \u{b7} \u{2191} {} row(s) back \u{b7} End returns to live",
            model.scroll_from_bottom
        ));
    }
    if model.queued_count > 0 {
        let plural = if model.queued_count == 1 { "" } else { "s" };
        text.push_str(&format!(
            " · {} message{} queued",
            model.queued_count, plural
        ));
    }
    // opencode status bar: a filled token badge that turns amber past 80% context.
    let mut spans = vec![Span::styled(text, Style::default().fg(BORDER))];
    if let Some((badge_text, pct)) = &model.token_badge {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            format!(" {badge_text} "),
            Style::default()
                .bg(if *pct >= 80 { WARN } else { TEXT })
                .fg(BG_PANEL)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// The transcript as it is actually drawn: every source line wrapped to `width`, each wrapped
/// row still carrying its owning card. Search re-uses this so the row it counts is the row that
/// gets rendered.
fn wrapped_transcript(model: &Model, width: u16) -> Vec<(Line<'static>, Option<u64>)> {
    let fp = wrapped_fingerprint(model);
    let cache = WRAPPED_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.as_ref()
        && cached.width == width
        && cached.fp == fp
    {
        return cached.rows.clone();
    }
    let mut rows = Vec::new();
    for (line, owner) in transcript_rows(model, width) {
        for row in wrap_spans(&line.spans, width.max(1) as usize) {
            rows.push((Line::from(row), owner));
        }
    }
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(WrappedCache {
            width,
            fp,
            rows: rows.clone(),
        });
    }
    rows
}

/// Repaints a whole drawn row onto `background`. Colouring only the matched substring would
/// mean re-deriving character offsets through styling and wrapping that have already been
/// applied; marking the row says the same thing and cannot drift out of step with it.
fn highlight_row(line: Line<'static>, background: Color) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|span| {
                let style = span.style.bg(background);
                Span::styled(span.content.to_string(), style)
            })
            .collect::<Vec<_>>(),
    )
}

/// Plain text of one drawn row, for matching a search against.
fn row_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Indices of the drawn rows containing `needle`, case-insensitively.
fn matching_rows(rows: &[(Line<'static>, Option<u64>)], needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle = needle.to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, (line, _))| row_text(line).to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

/// The `scroll_from_bottom` that puts `row` in the middle of a `visible`-row viewport.
fn scroll_to_row(total_rows: usize, visible: usize, row: usize) -> usize {
    let half = visible / 2;
    let start = row.saturating_sub(half);
    let max_start = total_rows.saturating_sub(visible);
    total_rows
        .saturating_sub(visible)
        .saturating_sub(start.min(max_start))
}

/// Every transcript line paired with the card it belongs to (`None` for notices and warnings,
/// which fold nothing and so are not click targets).
fn transcript_rows(model: &Model, width: u16) -> Vec<(Line<'static>, Option<u64>)> {
    let inner_width = width.max(20) as usize;
    let mut lines: Vec<(Line<'static>, Option<u64>)> = Vec::new();
    for card in &model.cards {
        let owner = (card.foldable_rows() > 0).then_some(card.id);
        for line in card_lines(card, inner_width) {
            lines.push((line, owner));
        }
        lines.push((Line::from(""), None));
    }
    // The agent's own progress reads as the next thing in the conversation, directly under the
    // last message, instead of as a label bolted onto the editor the user is typing in.
    if let Some((frame_idx, label)) = model.thinking {
        lines.push((
            Line::from(vec![
                Span::styled(THICK_BORDER.to_string(), Style::default().fg(BLUE)),
                Span::styled(
                    format!("{} ", SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()]),
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(label.to_string(), Style::default().fg(MUTED)),
            ]),
            None,
        ));
        lines.push((Line::from(""), None));
    }
    if model.warnings_visible {
        for warning in &model.warnings {
            lines.push((
                Line::styled(format!("\u{26a0} {warning}"), Style::default().fg(WARN)),
                None,
            ));
        }
        if model.warning_details_visible {
            for detail in &model.warning_details {
                lines.push((Line::from(""), None));
                lines.push((
                    Line::styled(format!("  \u{21b3} {detail}"), Style::default().fg(MUTED)),
                    None,
                ));
            }
        }
    }
    lines
}

/// OpenCode's chat grammar (internal/tui/components/chat/message.go): every message is a thick
/// left border with no background fill — user borders secondary-blue, assistant the brand
/// accent, tool calls muted — and raw output truncates to a head window with an expand hint.
const THICK_BORDER: &str = "\u{258c} ";
/// Rows shown for a folded card before the expand hint.
const COLLAPSED_PEEK: usize = 2;

/// One-line call header: the tool name plus a trimmed peek at its arguments, so a folded card
/// still says what ran and against what.
fn tool_header(name: &str, args: &str) -> String {
    let compact = args.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact = crate::tui::truncate_chars(&compact, 60);
    if compact.is_empty() {
        name.to_string()
    } else {
        format!("{name}  {compact}")
    }
}

fn card_lines(card: &Card, width: usize) -> Vec<Line<'static>> {
    let (border, body_style) = if card.title == "Assistant" {
        (BLUE, Style::default().fg(TEXT))
    } else {
        match card.kind {
            CardKind::User => (ACCENT, Style::default().fg(TEXT)),
            CardKind::Tool => match &card.status {
                None => (WARN, Style::default().fg(MUTED)),
                Some((_, true)) => (GREEN, Style::default().fg(MUTED)),
                Some((_, false)) => (RED, Style::default().fg(RED)),
            },
            CardKind::Output => match &card.status {
                Some((_, false)) => (RED, Style::default().fg(RED)),
                Some((_, true)) => (GREEN, Style::default().fg(MUTED)),
                None => (GREEN, Style::default().fg(MUTED)),
            },
            CardKind::Error => (RED, Style::default().fg(RED)),
            CardKind::Note => (BORDER, Style::default().fg(NOTICE_FG)),
        }
    };

    let mut out = Vec::new();
    let push_bordered = |out: &mut Vec<Line<'static>>, spans: Vec<Span<'static>>| {
        let mut line = vec![Span::styled(
            THICK_BORDER.to_string(),
            Style::default().fg(border),
        )];
        line.extend(spans);
        out.push(Line::from(line));
    };

    if card.title == "Assistant" && !card.collapsed {
        let text = crate::markdown::render_ratatui(&card.body);
        for line in text.lines {
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), span.style.patch(body_style)))
                .collect();
            push_bordered(&mut out, spans);
        }
        return out;
    }

    // Tool cards lead with their own header row. Without it a finished call reduced to a
    // bare outcome ("completed - 0ms - 332 chars") that never said which tool produced it.
    if matches!(card.kind, CardKind::Tool | CardKind::Note) && !card.title.is_empty() {
        push_bordered(
            &mut out,
            vec![Span::styled(
                card.title.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )],
        );
    }
    // The outcome stays visible whether or not the output is folded: how a tool ended is the
    // part worth reading at a glance, and it used to be a detached notice further down.
    if let Some((status, ok)) = &card.status {
        push_bordered(
            &mut out,
            vec![Span::styled(
                format!("{} {status}", if *ok { "\u{2713}" } else { "\u{2717}" }),
                Style::default().fg(if *ok { GREEN } else { RED }),
            )],
        );
    }

    let total_lines = card.body.lines().count();
    if card.collapsed {
        // A card that already shows an outcome needs no peek: it folds to two tidy rows and
        // opens on demand. Cards without one keep the old head window.
        let peek = if card.status.is_some() {
            0
        } else {
            COLLAPSED_PEEK
        };
        for source in card.body.lines().take(peek) {
            for row in wrap_display(source, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(row, body_style)]);
            }
        }
        let hidden = total_lines.saturating_sub(peek);
        if hidden > 0 && !card.body.trim().is_empty() {
            push_bordered(
                &mut out,
                vec![Span::styled(
                    format!("\u{2026} {hidden} more line(s) \u{b7} ctrl+o or click"),
                    Style::default().fg(GREEN),
                )],
            );
        }
        return out;
    }

    match card.kind {
        CardKind::User => {
            // A plain prompt carries no header; a labelled one (steering) says so above itself.
            if card.title != "User" && !card.title.is_empty() {
                push_bordered(
                    &mut out,
                    vec![Span::styled(
                        card.title.clone(),
                        Style::default().fg(MUTED).add_modifier(Modifier::DIM),
                    )],
                );
            }
            for line in wrap_display(&card.body, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(line, body_style)]);
            }
        }
        CardKind::Error => {
            for line in wrap_display(&card.body, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(line, body_style)]);
            }
        }
        _ => {
            // Tool call: "Name: args", then its output beneath, all under one muted rail.
            for (i, source) in card.body.lines().enumerate() {
                let styled = if i == 0 {
                    Span::styled(source.to_string(), Style::default().fg(TEXT))
                } else {
                    Span::styled(source.to_string(), body_style)
                };
                for row in wrap_display(styled.content.as_ref(), width.saturating_sub(4)) {
                    if i == 0 {
                        push_bordered(&mut out, vec![Span::styled(row, Style::default().fg(TEXT))]);
                    } else {
                        push_bordered(&mut out, vec![Span::styled(row, body_style)]);
                    }
                }
            }
        }
    }
    out
}

/// Char-greedy wrap of styled spans to `width` columns. Carries each source span's style into
/// the produced rows; soft-wrap only (newlines already split upstream).
/// Word-aware greedy wrap of styled spans to `width` columns. Breaks at spaces when a word
/// fits on the next row; hard-splits only words longer than the whole width. Styles carry
/// through every produced row.
fn wrap_spans(spans: &[Span<'_>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    #[derive(Clone)]
    enum Tok {
        Word(String, Style),
        Space(String, Style),
        Break,
    }
    let mut toks: Vec<Tok> = Vec::new();
    for span in spans {
        let style = span.style;
        let mut cur = String::new();
        let mut cur_is_space = false;
        for ch in span.content.chars() {
            if ch == '\n' {
                if !cur.is_empty() {
                    let done = std::mem::take(&mut cur);
                    toks.push(if cur_is_space {
                        Tok::Space(done, style)
                    } else {
                        Tok::Word(done, style)
                    });
                }
                toks.push(Tok::Break);
                cur_is_space = false;
                continue;
            } else {
                let is_sp = ch == ' ';
                if !cur.is_empty() && is_sp != cur_is_space {
                    let done = std::mem::take(&mut cur);
                    toks.push(if cur_is_space {
                        Tok::Space(done, style)
                    } else {
                        Tok::Word(done, style)
                    });
                }
                cur_is_space = is_sp;
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            toks.push(if cur_is_space {
                Tok::Space(cur, style)
            } else {
                Tok::Word(cur, style)
            });
        }
    }

    fn tok_width(t: &str) -> usize {
        t.chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let push_row_text = |rows: &mut Vec<Vec<Span<'static>>>, text: String, style: Style| {
        rows.last_mut().unwrap().push(Span::styled(text, style));
    };

    let mut used = 0usize;
    for tok in &toks {
        match tok {
            Tok::Break => {
                rows.push(Vec::new());
                used = 0;
            }
            Tok::Space(text, style) => {
                if used == 0 {
                    continue; // no leading spaces after a wrap
                }
                let w = tok_width(text);
                if used + w > width {
                    continue; // spaces vanish at end of row
                }
                used += w;
                push_row_text(&mut rows, text.clone(), *style);
            }
            Tok::Word(text, style) => {
                let w = tok_width(text);
                if w > width {
                    // Hard-split oversized words by display width.
                    let mut rest = text.as_str();
                    loop {
                        if used >= width {
                            rows.push(Vec::new());
                            used = 0;
                        }
                        let avail = width - used;
                        let mut take_w = 0usize;
                        let mut take_end = 0usize;
                        for (i, ch) in rest.char_indices() {
                            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                            if take_w + cw > avail {
                                take_end = i;
                                break;
                            }
                            take_w += cw;
                            take_end = i + ch.len_utf8();
                        }
                        if take_end == 0 {
                            take_end = rest.ceil_char_boundary(1.min(rest.len()));
                        }
                        let piece = &rest[..take_end];
                        push_row_text(&mut rows, piece.to_string(), *style);
                        used += take_w;
                        rest = &rest[take_end..];
                        if rest.is_empty() {
                            break;
                        }
                    }
                } else {
                    if used + w > width && used > 0 {
                        rows.push(Vec::new());
                        used = 0;
                    }
                    used += w;
                    push_row_text(&mut rows, text.clone(), *style);
                }
            }
        }
    }
    rows
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

    /// Draws one frame into a test backend and reports where the terminal caret ended up
    /// together with the row it landed on, so caret and text can be compared directly.
    fn caret_and_row(model: &Model) -> ((u16, u16), String) {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, model);
            })
            .expect("draw");
        let pos = terminal.get_cursor_position().expect("caret");
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..60).map(|x| buffer[(x, pos.y)].symbol()).collect();
        ((pos.x, pos.y), row)
    }

    /// Collects a card's rendered rows as plain strings.
    fn rendered(card: &Card, width: usize) -> Vec<String> {
        card_lines(card, width)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    fn note(id: u64, title: &str, body: &str) -> Card {
        Card {
            id,
            kind: CardKind::Note,
            title: title.to_string(),
            body: body.to_string(),
            status: None,
            collapsed: false,
        }
    }

    #[test]
    fn folding_targets_the_newest_card_that_has_something_folded() {
        // A command leaves a note cell behind as the literal last card. `/expand` used to aim
        // at that instead of the tool output the user had just seen.
        let cards = vec![
            note(1, "", "no body to speak of"),
            Card {
                id: 2,
                kind: CardKind::Tool,
                title: "read_file".into(),
                body: "a\nb\nc".into(),
                status: Some(("completed".into(), true)),
                collapsed: true,
            },
            note(3, "/sessions", ""),
        ];
        assert_eq!(
            last_foldable_index(&cards),
            Some(1),
            "the tool card, not the trailing note"
        );
        assert_eq!(last_foldable_index(&[]), None);
        assert_eq!(
            last_foldable_index(&[note(1, "/help", "")]),
            None,
            "a cell with no body folds nothing"
        );
    }

    #[test]
    fn copying_an_answer_yields_the_raw_markdown() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Assistant".into(),
            body: "# Heading\n\nsome **text**".into(),
            status: None,
            collapsed: false,
        };
        // No rails, no header: what is copied is what the model wrote.
        assert_eq!(card.clipboard_text(), "# Heading\n\nsome **text**");
    }

    #[test]
    fn copying_a_tool_cell_keeps_what_says_the_body_is() {
        let card = Card {
            id: 1,
            kind: CardKind::Tool,
            title: tool_header("read_file", r#"{"path": "src/main.rs"}"#),
            body: "fn main() {}".into(),
            status: Some(("completed \u{b7} 3ms \u{b7} 12 chars".into(), true)),
            collapsed: true,
        };
        let text = card.clipboard_text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("read_file"), "{text:?}");
        assert!(lines[1].contains("completed"), "{text:?}");
        assert_eq!(lines[2], "fn main() {}");
    }

    #[test]
    fn a_command_and_its_output_render_as_one_cell() {
        let card = note(1, "/sessions", "row one\nrow two");
        let rows = rendered(&card, 60);
        assert_eq!(rows.len(), 3, "header plus both rows: {rows:?}");
        assert!(
            rows[0].contains("/sessions"),
            "the command heads the cell: {rows:?}"
        );
        assert!(
            rows[1].contains("row one") && rows[2].contains("row two"),
            "{rows:?}"
        );
        for row in &rows {
            assert!(
                row.starts_with('\u{258c}'),
                "every row carries the cell rail: {row:?}"
            );
        }
    }

    #[test]
    fn command_cells_keep_their_place_in_the_transcript() {
        // Notices used to render after every card regardless of when they happened, so command
        // output always sank to the bottom instead of sitting where it was produced.
        let model = Model {
            intro: false,
            cards: vec![
                note(1, "/model ornith", "Now using ornith:latest (ornith)."),
                Card {
                    id: 2,
                    kind: CardKind::User,
                    title: "User".into(),
                    body: "halo".into(),
                    status: None,
                    collapsed: false,
                },
                note(3, "/compact", "Not enough history to compact yet."),
            ],
            ..Default::default()
        };
        let rows: Vec<String> = transcript_rows(&model, 60)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let position = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {rows:?}"))
        };
        assert!(position("/model ornith") < position("halo"));
        assert!(position("halo") < position("/compact"));
    }

    #[test]
    fn a_running_turn_reports_itself_under_the_transcript() {
        // The spinner belongs where the answer will appear, not welded to the editor row.
        let model = Model {
            intro: false,
            cards: vec![note(1, "", "earlier output")],
            thinking: Some((0, "Thinking...")),
            ..Default::default()
        };
        let rows: Vec<String> = transcript_rows(&model, 60)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let spinner = rows
            .iter()
            .position(|row| row.contains("Thinking..."))
            .expect("spinner row present");
        let output = rows
            .iter()
            .position(|row| row.contains("earlier output"))
            .expect("earlier output present");
        assert!(spinner > output, "the run trails the transcript: {rows:?}");
    }

    #[test]
    fn the_editor_stays_usable_while_a_turn_runs() {
        let model = Model {
            intro: false,
            input: "steer me".into(),
            input_caret: 8,
            thinking: Some((3, "Thinking...")),
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let caret = terminal.get_cursor_position().expect("caret");
        let buffer = terminal.backend().buffer().clone();
        let row: String = (0..80).map(|x| buffer[(x, caret.y)].symbol()).collect();
        // The buffer is still shown and the caret still sits at its end, rather than the editor
        // being replaced by a status label for the duration of the turn.
        assert!(row.contains("steer me"), "buffer stays visible: {row:?}");
        assert_eq!(caret.x, "\u{2502}\u{276f} steer me".chars().count() as u16);
        let all: Vec<String> = (0..14)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            all.iter().any(|row| row.contains("Esc interrupts")),
            "the loading row says how to stop: {all:?}"
        );
    }

    fn rows_of(texts: &[&str]) -> Vec<(Line<'static>, Option<u64>)> {
        texts
            .iter()
            .map(|text| (Line::from(Span::raw(text.to_string())), None))
            .collect()
    }

    fn with_sidebar(width: u16, hidden: bool) -> RenderInfo {
        let model = Model {
            intro: false,
            sidebar_hidden: hidden,
            sidebar: Some(vec![("Model".into(), "orvix/auto".into())]),
            cards: vec![Card {
                id: 1,
                kind: CardKind::User,
                title: "User".into(),
                body: "hi".into(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(width, 14);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| {
                info = render(frame, &model);
            })
            .expect("draw");
        info
    }

    #[test]
    fn the_sidebar_narrows_before_it_disappears() {
        // Dropping the rail outright at 84 columns took the session, model, and mode readout
        // with it and said nothing about why.
        assert_eq!(
            with_sidebar(100, false).transcript_width,
            70,
            "wide: full 30-column rail"
        );
        assert_eq!(
            with_sidebar(76, false).transcript_width,
            52,
            "tight: narrowed to 24"
        );
        assert_eq!(
            with_sidebar(60, false).transcript_width,
            60,
            "too narrow for any rail: the transcript takes the whole width"
        );
    }

    #[test]
    fn hiding_the_sidebar_gives_its_columns_to_the_transcript() {
        assert_eq!(with_sidebar(100, true).transcript_width, 100);
    }

    #[test]
    fn a_steering_message_is_labelled_as_one() {
        let steer = Card {
            id: 1,
            kind: CardKind::User,
            title: "steering \u{2192} added to the running turn".into(),
            body: "actually use the other file".into(),
            status: None,
            collapsed: false,
        };
        let rows = rendered(&steer, 60);
        assert!(
            rows[0].contains("steering"),
            "the label heads the cell: {rows:?}"
        );
        assert!(rows[1].contains("actually use"), "{rows:?}");

        // An ordinary prompt gains no header from this.
        let plain = Card {
            id: 2,
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            status: None,
            collapsed: false,
        };
        assert_eq!(
            rendered(&plain, 60).len(),
            1,
            "no header for a plain prompt"
        );
    }

    #[test]
    fn search_matches_rows_case_insensitively() {
        let rows = rows_of(&["the Cargo build", "nothing here", "cargo test", "CARGO"]);
        assert_eq!(matching_rows(&rows, "cargo"), vec![0, 2, 3]);
        assert_eq!(
            matching_rows(&rows, "CARGO"),
            vec![0, 2, 3],
            "either case finds either"
        );
        assert!(matching_rows(&rows, "absent").is_empty());
        assert!(
            matching_rows(&rows, "").is_empty(),
            "an empty query matches nothing, not everything"
        );
    }

    #[test]
    fn search_matches_text_split_across_styled_spans() {
        // Rows are built from many spans (rail, header, body). Matching per span would miss a
        // word that straddles two of them.
        let line = Line::from(vec![
            Span::raw("\u{258c} ".to_string()),
            Span::raw("car".to_string()),
            Span::raw("go build".to_string()),
        ]);
        let rows = vec![(line, None)];
        assert_eq!(matching_rows(&rows, "cargo build"), vec![0]);
    }

    #[test]
    fn scrolling_to_a_match_centres_it_and_stays_in_range() {
        // 100 rows, a 20-row viewport. scroll_from_bottom counts up from the live tail.
        assert_eq!(
            scroll_to_row(100, 20, 90),
            0,
            "a match in the tail needs no scrolling"
        );
        assert_eq!(
            scroll_to_row(100, 20, 50),
            40,
            "40 rows back puts row 50 mid-viewport"
        );
        assert_eq!(
            scroll_to_row(100, 20, 0),
            80,
            "the first row scrolls all the way back"
        );
        // Fewer rows than fit: nothing to scroll, and no underflow.
        assert_eq!(scroll_to_row(5, 20, 3), 0);
        assert_eq!(scroll_to_row(0, 20, 0), 0);
    }

    #[test]
    fn highlighting_a_row_repaints_it_without_losing_its_text() {
        let line = Line::from(vec![
            Span::styled("\u{258c} ".to_string(), Style::default().fg(BLUE)),
            Span::styled("hit".to_string(), Style::default().fg(TEXT)),
        ]);
        let painted = highlight_row(line, MATCH_BG);
        assert_eq!(row_text(&painted), "\u{258c} hit", "text survives");
        for span in &painted.spans {
            assert_eq!(span.style.bg, Some(MATCH_BG), "every span carries the wash");
        }
        // Foreground styling is left alone, so a highlighted answer still reads as an answer.
        assert_eq!(painted.spans[0].style.fg, Some(BLUE));
        assert_eq!(painted.spans[1].style.fg, Some(TEXT));
    }

    #[test]
    fn caret_motion_walks_characters_words_and_lines() {
        let buf = "hello brave world";
        assert_eq!(prev_char_boundary(buf, 5), 4);
        assert_eq!(prev_char_boundary(buf, 0), 0, "start of buffer is a floor");
        assert_eq!(next_char_boundary(buf, 16), 17);
        assert_eq!(
            next_char_boundary(buf, 17),
            17,
            "end of buffer is a ceiling"
        );

        // Alt+Left from the end lands at the start of the last word, and again at the one
        // before it -- the run of spaces is skipped, not counted as a word.
        assert_eq!(prev_word_boundary(buf, buf.len()), 12);
        assert_eq!(prev_word_boundary(buf, 12), 6);
        assert_eq!(prev_word_boundary(buf, 0), 0);
        assert_eq!(next_word_boundary(buf, 0), 5);
        assert_eq!(next_word_boundary(buf, buf.len()), buf.len());
    }

    #[test]
    fn caret_motion_is_utf8_safe() {
        // Byte stepping would slice a multi-byte char in half and panic.
        let buf = "haló界";
        let mut caret = buf.len();
        let mut steps = 0;
        while caret > 0 {
            caret = prev_char_boundary(buf, caret);
            steps += 1;
            assert!(
                buf.is_char_boundary(caret),
                "landed mid-character at {caret}"
            );
        }
        assert_eq!(steps, 5, "five characters, not eight bytes");
    }

    #[test]
    fn home_and_end_act_on_the_buffer_line_under_the_caret() {
        let buf = "first\nsecond\nthird";
        let caret = buf.find("second").unwrap() + 2;
        assert_eq!(line_start(buf, caret), buf.find("second").unwrap());
        assert_eq!(line_end(buf, caret), buf.find("second").unwrap() + 6);
        assert_eq!(
            line_start(buf, 2),
            0,
            "first line starts at the buffer start"
        );
        assert_eq!(line_end(buf, buf.len()), buf.len());
    }

    #[test]
    fn a_long_line_scrolls_to_keep_the_caret_visible() {
        // Editing in the middle of a line longer than the box must not push the caret off it.
        let buf = "x".repeat(200);
        let view = editor_view(&buf, 120, 40);
        assert_eq!(view.rows.len(), 1);
        assert!(
            view.caret_col < 40,
            "caret stays inside the box: {}",
            view.caret_col
        );
        assert!(view.rows[0].chars().count() <= 40, "row fits the box");

        // At the very start the window shows the head, with the caret at column 0.
        let head = editor_view(&buf, 0, 40);
        assert_eq!(head.caret_col, 0);
        assert!(head.rows[0].starts_with('x'));
    }

    #[test]
    fn the_view_scrolls_up_to_reach_a_caret_on_an_earlier_line() {
        // Eight lines with the caret on the first: the newest-lines window alone would leave
        // the caret off screen, so the window has to extend back to it.
        let buf = (1..=8)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = editor_view(&buf, 2, 40);
        assert_eq!(view.caret_row, 0, "the caret's line is the first shown");
        assert!(view.rows[0].contains("line 1"), "{:?}", view.rows);
        assert_eq!(view.caret_col, 2);
    }

    #[test]
    fn folded_tool_card_still_names_the_tool_and_its_outcome() {
        // The reported problem: a finished call collapsed to a bare "completed - 0ms - 332
        // chars" with no way to tell which tool it belonged to.
        let card = Card {
            id: 7,
            kind: CardKind::Tool,
            title: tool_header("read_file", r#"{"path": "src/main.rs"}"#),
            body: (1..=20)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            status: Some(("completed \u{b7} 28.0s \u{b7} 142 chars".into(), true)),
            collapsed: true,
        };
        let rows = rendered(&card, 70);
        assert_eq!(rows.len(), 3, "header + outcome + fold hint: {rows:?}");
        assert!(
            rows[0].contains("read_file"),
            "header names the tool: {rows:?}"
        );
        assert!(
            rows[0].contains("src/main.rs"),
            "header peeks at args: {rows:?}"
        );
        assert!(
            rows[1].contains("completed"),
            "outcome stays visible: {rows:?}"
        );
        assert!(
            rows[1].contains('\u{2713}'),
            "outcome is marked as success: {rows:?}"
        );
        assert!(
            rows[2].contains("20 more line(s)"),
            "everything else folds: {rows:?}"
        );
    }

    #[test]
    fn failed_tool_card_marks_the_outcome_and_keeps_the_detail_foldable() {
        let card = Card {
            id: 8,
            kind: CardKind::Tool,
            title: tool_header("run_command", r#"{"command": "cargo test"}"#),
            body: "thread 'main' panicked\nstack backtrace follows".into(),
            status: Some(("failed \u{b7} 1.2s".into(), false)),
            collapsed: true,
        };
        let rows = rendered(&card, 70);
        assert!(rows[1].contains('\u{2717}'), "failure is marked: {rows:?}");
        assert!(
            rows[1].contains("failed"),
            "outcome says it failed: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.contains("panicked")),
            "detail stays folded until asked for: {rows:?}"
        );
    }

    #[test]
    fn unfolding_a_card_reveals_the_body_under_the_outcome() {
        let mut card = Card {
            id: 9,
            kind: CardKind::Tool,
            title: tool_header("grep", "pattern"),
            body: "hit one\nhit two".into(),
            status: Some(("completed \u{b7} 3ms \u{b7} 15 chars".into(), true)),
            collapsed: false,
        };
        let rows = rendered(&card, 70);
        assert!(rows.iter().any(|row| row.contains("hit one")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("hit two")), "{rows:?}");
        card.collapsed = true;
        assert!(
            rendered(&card, 70)
                .iter()
                .all(|row| !row.contains("hit one")),
            "folding hides the body again"
        );
    }

    #[test]
    fn drawn_rows_map_back_to_their_card_for_click_targeting() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 42,
                kind: CardKind::Tool,
                title: tool_header("glob", "**/*.rs"),
                body: (1..=6)
                    .map(|i| format!("file {i}.rs"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                status: Some(("completed \u{b7} 9ms \u{b7} 60 chars".into(), true)),
                collapsed: true,
            }],
            ..Default::default()
        };
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut info = RenderInfo::default();
        terminal
            .draw(|frame| {
                info = render(frame, &model);
            })
            .expect("draw");
        assert!(!info.card_rows.is_empty(), "card rows are recorded");
        assert!(
            info.card_rows.iter().all(|(_, id)| *id == 42),
            "every recorded row belongs to the only card"
        );
        // A card with nothing folded away is not a click target.
        let plain = Model {
            intro: false,
            cards: vec![Card {
                id: 43,
                kind: CardKind::User,
                title: "User".into(),
                body: String::new(),
                status: None,
                collapsed: false,
            }],
            ..Default::default()
        };
        terminal
            .draw(|frame| {
                info = render(frame, &plain);
            })
            .expect("draw");
        assert!(
            info.card_rows.is_empty(),
            "nothing to expand, nothing to click"
        );
    }

    #[test]
    fn caret_lands_on_the_row_that_holds_the_typed_text() {
        // Reproduces the slash-menu report: with `/st` typed the caret sat one row below the
        // text and one column short of its end, because the block draws no top border and the
        // "\u{276f} " prefix is two cells wide.
        let model = Model {
            intro: false,
            input: "/st".into(),
            input_caret: 3,
            ac_items: vec![
                ("stats".into(), "Show current session usage".into()),
                ("status".into(), "Show project and connection status".into()),
            ],
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert!(
            row.starts_with("\u{2502}\u{276f} /st"),
            "caret row holds the buffer: {row:?}"
        );
        let buffer_end = ("\u{2502}\u{276f} /st".chars().count()) as u16;
        assert_eq!(
            x, buffer_end,
            "caret sits immediately after the last character"
        );
        // And the cell under the caret is still empty, i.e. it did not land on a glyph.
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        assert_eq!(terminal.backend().buffer()[(x, y)].symbol(), " ");
    }

    #[test]
    fn caret_follows_the_last_line_of_a_multiline_buffer() {
        let model = Model {
            intro: false,
            input: "first\nsecond".into(),
            input_caret: "first\nsecond".len(),
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert!(
            row.starts_with("\u{2502}  second"),
            "caret row is the last segment: {row:?}"
        );
        assert_eq!(x, "\u{2502}  second".chars().count() as u16);
        // The earlier segment keeps its own row rather than being joined onto this one.
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render(frame, &model);
            })
            .expect("draw");
        let above: String = (0..60)
            .map(|col| terminal.backend().buffer()[(col, y - 1)].symbol())
            .collect();
        assert!(
            above.starts_with("\u{2502}\u{276f} first"),
            "previous segment on its own row: {above:?}"
        );
    }

    #[test]
    fn caret_stays_inside_the_editor_when_the_buffer_ends_with_a_newline() {
        let model = Model {
            intro: false,
            input: "done\n".into(),
            input_caret: "done\n".len(),
            ..Default::default()
        };
        let ((x, y), row) = caret_and_row(&model);
        assert_eq!(
            x, 3,
            "caret rests on the continuation indent of the empty last line"
        );
        assert!(
            row.trim_start_matches('\u{2502}').trim().is_empty(),
            "last row is the empty segment: {row:?}"
        );
        // Height is derived from the same split, so the caret cannot fall past the editor.
        assert!(y < 12);
    }

    #[test]
    fn wrapping_uses_display_width() {
        let rows = wrap_display("abc界def", 5);
        assert_eq!(rows, vec!["abc界", "def"]);
    }

    #[test]
    fn user_cards_use_thick_left_border() {
        let card = Card {
            id: 1,
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            status: None,
            collapsed: false,
        };
        let lines = card_lines(&card, 20);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "\u{258c} ");
        assert_eq!(lines[0].spans[0].style.fg, Some(ACCENT));
    }

    #[test]
    fn a_long_approval_body_scrolls_instead_of_being_cut() {
        // A patch diff is routinely longer than the modal. The body was cut at ten rows with
        // nothing saying so, which is being asked to authorise a change you cannot finish
        // reading.
        let body = (1..=40)
            .map(|i| format!("+ line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let perm = PermissionState {
            title: "Allow patch_file?".into(),
            body,
            selected: 0,
            scroll: 0,
        };
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_permission(frame, &perm, area);
            })
            .expect("draw");
        let screen: String = {
            let buffer = terminal.backend().buffer().clone();
            (0..30)
                .map(|y| {
                    (0..80)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            screen.contains("of 40 lines"),
            "the modal admits what it hid:\n{screen}"
        );
        assert!(screen.contains("PgUp/PgDn"), "and says how to see the rest");
        assert!(screen.contains("line 1"), "the body starts at the top");

        // Scrolled down, later lines come into view.
        let scrolled = PermissionState { scroll: 20, ..perm };
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_permission(frame, &scrolled, area);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..30)
            .map(|y| {
                (0..80)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.contains("line 30"),
            "scrolling reaches later lines:\n{screen}"
        );
    }

    #[test]
    fn assistant_cards_render_unboxed() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Assistant".into(),
            body: "hello **world**".into(),
            status: None,
            collapsed: false,
        };
        let lines = card_lines(&card, 40);
        assert!(!lines.is_empty());
        // Every line starts with the accent bar and carries no background fill.
        for line in &lines {
            assert_eq!(line.spans[0].content, "\u{258c} ");
            assert_eq!(line.spans[0].style.fg, Some(BLUE));
            assert!(line.spans[0].style.bg.is_none());
        }
    }

    #[test]
    fn collapsed_tool_output_shows_head_window_and_hint() {
        let card = Card {
            id: 1,
            kind: CardKind::Output,
            title: "Tool Output".into(),
            body: (1..=15)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            status: None,
            collapsed: true,
        };
        let lines = card_lines(&card, 60);
        assert_eq!(lines.len(), COLLAPSED_PEEK + 1);
        let last = lines.last().unwrap();
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("13 more line(s)"));
        assert!(text.contains("ctrl+o"), "fold hint names the key: {text:?}");
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn wrap_spans_splits_on_newline() {
        let spans = vec![Span::raw("aaa\nbbb\nccc")];
        let rows = wrap_spans(&spans, 20);
        assert_eq!(rows.len(), 3, "rows: {rows:?}");
    }
}

#[cfg(test)]
mod line_tests {
    use super::*;

    /// ratatui 0.30 strips embedded newlines from Span content at construction. This is WHY
    /// multi-line notices are split into separate Lines in transcript_rows.
    #[test]
    fn line_styled_strips_embedded_newlines() {
        let line = Line::styled("a\nb\nc".to_string(), Style::default());
        let txt: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(txt.matches('\n').count(), 0);
    }

    #[test]
    fn multiline_notices_become_separate_lines() {
        let model = Model {
            intro: false,
            cards: vec![Card {
                id: 1,
                kind: CardKind::Note,
                title: String::new(),
                body: "line one\nline two".to_string(),
                status: None,
                collapsed: false,
            }],
            ..Model::default()
        };
        let rendered: Vec<String> = transcript_rows(&model, 80)
            .iter()
            .map(|(line, _)| line.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        // Each source line becomes its own row (the leading span is the cell's rail).
        assert!(
            rendered.iter().any(|row| row.ends_with("line one")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|row| row.ends_with("line two")),
            "{rendered:?}"
        );
    }
}
