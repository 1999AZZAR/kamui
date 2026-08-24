use crate::terminal::{Style as AnsiStyle, Ui};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
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

// OpenCode dark palette (theme/assets/opencode.json) with kamui's blue as the brand accent.
const TEXT: Color = Color::Rgb(0xee, 0xee, 0xee);
const MUTED: Color = Color::Rgb(0x80, 0x80, 0x80);
const BORDER: Color = Color::Rgb(0x48, 0x48, 0x48);
const BG_ELEMENT: Color = Color::Rgb(0x1e, 0x1e, 0x1e);
const BG_PANEL: Color = Color::Rgb(0x14, 0x14, 0x14);
const BLUE: Color = Color::Rgb(0x5c, 0x9c, 0xf5);
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
    /// Autocomplete menu state mirrored from the input loop each keystroke.
    ac_items: Vec<(String, String)>,
    ac_selected: usize,
    /// Warning messages render separately so `/warnings` can hide or reveal them.
    warnings: Vec<String>,
    warnings_visible: bool,
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
            notices: Vec::new(),
            footer: String::from(
                "! shell · / commands · Tab · \u{2191} history · PgUp/PgDn scroll · Ctrl+C cancel",
            ),
            scroll_from_bottom: 0,
            prompt_visible: true,
            thinking: None,
            intro: true,
            sidebar: None,
            input: String::new(),
            ac_items: Vec::new(),
            ac_selected: 0,
            warnings: Vec::new(),
            warnings_visible: true,
            queued_count: 0,
            dialog: None,
            help_visible: false,
            token_badge: None,
            permission: None,
        }
    }
}

struct FullScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    model: Model,
    /// Transcript viewport height from the most recent draw; PageUp/PageDown use it as the
    /// scroll page size.
    last_viewport_rows: usize,
}

impl FullScreen {
    fn new(header: String) -> Result<Self> {
        // If anything panics mid-draw, still restore the terminal instead of leaving it raw.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            previous_hook(info);
        }));
        let mut stdout = io::stdout();
        enable_raw_mode().context("could not enable raw mode")?;
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
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
        };
        screen.draw()?;
        Ok(screen)
    }

    fn draw(&mut self) -> Result<()> {
        let model = self.model.clone();
        let mut viewport = 0usize;
        self.terminal.draw(|frame| {
            viewport = render(frame, &model);
        })?;
        self.last_viewport_rows = viewport;
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
        // Agent noise starts folded: every tool call collapses to a two-line peek, and any
        // output longer than two lines joins it. `/expand` // `/collapse` toggle the last
        // card; answers and errors always show in full.
        let collapsed = match kind {
            CardKind::Tool => true,
            CardKind::Output => title != "Assistant" && body.lines().count() > 2,
            _ => false,
        };
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
        if self.model.notices.len() > 6 {
            self.model.notices.remove(0);
        }
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

impl Drop for FullScreen {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
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
fn input_tail(input: &str, max: usize) -> &str {
    let mut start = 0usize;
    let mut used = 0usize;
    for (i, ch) in input.char_indices().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
        if used + cw > max {
            start = i + ch.len_utf8();
            break;
        }
        used += cw;
    }
    &input[start..]
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
        Paragraph::new(Text::from(lines)).block(
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
    let body_rows: Vec<String> = wrap_display(&perm.body, width.saturating_sub(6) as usize)
        .into_iter()
        .take(10)
        .collect();
    let height = (body_rows.len() as u16 + PERM_OPTIONS.len() as u16 + 5)
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
        lines.push(Line::styled(row.clone(), Style::default().fg(TEXT)));
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
        Paragraph::new(Text::from(lines)).block(
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
fn help_overlay() -> Paragraph<'static> {
    let rows: [(&str, &str); 12] = [
        ("Enter", "send message"),
        ("Ctrl+K", "switch model"),
        ("Ctrl+S", "resume a session"),
        ("?", "toggle this help"),
        ("Tab", "accept slash completion"),
        ("\u{2191}/\u{2193}", "history / menu navigation"),
        ("PgUp/PgDn", "scroll transcript"),
        ("Home/End", "jump to top/bottom"),
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
    Paragraph::new(Text::from(lines)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BLUE)),
    )
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
    let mut selected = 0usize;
    let mut history: Vec<String> = Vec::new();
    let mut history_idx = 0usize;
    let mut saved_buf = String::new();
    let mut last_ctrl_c: Option<std::time::Instant> = None;

    let sync = |screen: &ScreenHandle, buf: &str, selected: usize, items: Vec<(String, String)>| {
        let mut s = lock_screen(&screen.0);
        s.model.input = buf.to_string();
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

    sync(&screen, "", 0, Vec::new());
    // Feed loop: the wheel scrolls right here; only key presses fall through to the editor.
    // Read errors are tolerated briefly, then quit gracefully (never process::exit - that
    // would skip FullScreen's Drop and leave raw mode + mouse capture enabled).
    let mut feed_errors = 0u32;
    'keys: loop {
        let key = 'feed: {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => break 'feed Some(key),
                Ok(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => scroll_screen(&screen, 3),
                    MouseEventKind::ScrollDown => scroll_screen(&screen, -3),
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
                    selected = 0;
                }
            }
            KeyCode::Tab => {
                if is_slash {
                    let all = items_for(&needle);
                    if let Some(choice) = all.get(selected) {
                        buf = format!("/{} ", choice.0);
                        selected = 0;
                    }
                }
            }
            KeyCode::Enter => {
                // opencode editor behavior: a trailing backslash escapes the newline and
                // continues the message on the next line.
                if buf.ends_with('\\') {
                    buf.pop();
                    buf.push('\n');
                    selected = 0;
                    continue 'keys;
                }
                let line = buf.trim().to_string();
                buf.clear();
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
                    selected = 0;
                    let mut s = lock_screen(&screen.0);
                    s.model.notices.push("interrupt requested".to_string());
                    let _ = s.draw();
                } else {
                    buf.clear();
                    selected = 0;
                }
            }
            KeyCode::PageUp => scroll_screen(&screen, page_rows(&screen)),
            KeyCode::PageDown => scroll_screen(&screen, -page_rows(&screen)),
            KeyCode::Home => scroll_screen(&screen, 100_000),
            KeyCode::End => scroll_screen(&screen, -100_000),
            KeyCode::Backspace => {
                buf.pop();
                selected = 0;
                if history_idx == history.len() {
                    saved_buf = buf.clone();
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
                    let mut sc = lock_screen(&screen.0);
                    sc.model
                        .notices
                        .push("Press Ctrl+C again within 3s to quit.".to_string());
                    let _ = sc.draw();
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
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
        sync(&screen, &buf, selected, items);
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
        s.model.notices.push(format!("queued: {line}"));
        drop(s);
        let _ = screen.draw_now();
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

/// Returns how many transcript rows are visible, so the input loop can page by a real
/// viewport.
fn render(frame: &mut Frame<'_>, model: &Model) -> usize {
    // OpenCode layout: transcript on top, autocomplete menu above the bordered editor, a
    // one-line footer, and the sidebar rail splitting the body horizontally.
    let popup_height = menu_height(model.ac_items.len());
    // Multiline editor: grows with the buffer's newlines (backslash-newline continuation).
    let input_lines = model.input.lines().count().max(1);
    let editor_rows = (input_lines as u16).min(6) + 2;
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
        // Wrap every row ourselves so viewport math is exact (Paragraph's own wrap happens
        // after scroll offsets, which makes bottom-follow drift on long wrapped lines).
        let transcript = transcript_text(model, transcript_area.width);
        let mut rows: Vec<Line<'static>> = Vec::with_capacity(transcript.lines.len());
        for line in &transcript.lines {
            for row in wrap_spans(&line.spans, transcript_area.width.max(1) as usize) {
                rows.push(Line::from(row));
            }
        }
        let visible = transcript_area.height as usize;
        let start = rows
            .len()
            .saturating_sub(visible.saturating_add(model.scroll_from_bottom));
        let window: Vec<Line<'static>> = rows.into_iter().skip(start).take(visible).collect();
        frame.render_widget(Paragraph::new(Text::from(window)), transcript_area);
    }
    if let Some(area) = sidebar_area {
        frame.render_widget(sidebar_paragraph(model, area), area);
    }
    if popup_height > 0 {
        frame.render_widget(popup_widget(model), popup_area);
    }
    frame.render_widget(editor_widget(model, editor_area), editor_area);

    // Terminal cursor sits at the end of the typed text whenever the editor owns input.
    if model.thinking.is_none() && !model.intro {
        let inner = editor_area.width.saturating_sub(4).max(1) as usize;
        let segments: Vec<&str> = model.input.split('\n').collect();
        let last = segments.last().copied().unwrap_or("");
        let row = editor_area.y + 1 + segments.len().saturating_sub(1).min(4) as u16;
        let col =
            editor_area.x + 2 + UnicodeWidthStr::width(input_tail(last, inner)).min(inner) as u16;
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
        frame.render_widget(help_overlay(), frame.area());
    }
    if let Some(dialog) = &model.dialog {
        render_dialog(frame, dialog, frame.area());
    }

    if model.intro && model.cards.is_empty() {
        0
    } else {
        body_area.height as usize
    }
}

/// The opencode-style prompt: left accent border, element background, `❯` glyph with the live
/// buffer, and a meta line pairing session info with key hints.
fn editor_widget(model: &Model, area: Rect) -> Paragraph<'static> {
    // Horizontal viewport: keep the caret (always at the end of the buffer) on screen.
    let thinking = match model.thinking {
        Some((frame_idx, label)) => Line::from(vec![
            Span::styled(
                format!("{} ", SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()]),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(MUTED)),
        ]),
        None if model.input.is_empty() => Line::from(vec![
            Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "type a message, or / for commands".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        None => {
            // Render every newline segment; viewport shows the last few lines.
            let inner = area.width.saturating_sub(4).max(1) as usize;
            let segments: Vec<&str> = model.input.split('\n').collect();
            let first = segments.len().saturating_sub(5);
            let mut spans = vec![Span::styled(
                "\u{276f} ".to_string(),
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
            )];
            for (i, seg) in segments[first..].iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(
                    if i + 1 == segments[first..].len() {
                        input_tail(seg, inner).to_string()
                    } else {
                        (*seg).to_string()
                    },
                    Style::default().fg(TEXT),
                ));
            }
            Line::from(spans)
        }
    };
    // Session info only; keybind hints live in the footer so long paths
    // never collide with them.
    let meta = Line::styled(
        crate::tui::truncate_chars(&model.header, 72),
        Style::default().fg(MUTED),
    );
    Paragraph::new(Text::from(vec![thinking, meta]))
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
    Paragraph::new(Text::from(lines)).style(Style::default().bg(BG_PANEL))
}

fn sidebar_paragraph(model: &Model, area: Rect) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(entries) = &model.sidebar {
        for (key, value) in entries {
            lines.push(Line::from(Span::styled(
                format!("{key} "),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::styled(
                crate::tui::truncate_chars(value, area.width.saturating_sub(4) as usize),
                Style::default().fg(NOTICE_FG),
            ));
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
    for notice in &model.notices {
        for row in notice.split('\n') {
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

fn transcript_text(model: &Model, width: u16) -> Text<'static> {
    let inner_width = width.max(20) as usize;
    let mut lines = Vec::new();
    for card in &model.cards {
        lines.extend(card_lines(card, inner_width));
        lines.push(Line::from(""));
    }
    // Status notes render as quiet plain lines, opencode-style. Split on newlines here:
    // ratatui Span content silently drops embedded newlines, so they must become
    // separate Lines before construction.
    for notice in &model.notices {
        for row in notice.split('\n') {
            lines.push(Line::styled(row.to_string(), Style::default().fg(MUTED)));
        }
    }
    if model.warnings_visible {
        for warning in &model.warnings {
            lines.push(Line::styled(
                format!("\u{26a0} {warning}"),
                Style::default().fg(WARN),
            ));
        }
    }
    Text::from(lines)
}

/// OpenCode's chat grammar (internal/tui/components/chat/message.go): every message is a thick
/// left border with no background fill — user borders secondary-blue, assistant the brand
/// accent, tool calls muted — and raw output truncates to a head window with an expand hint.
const THICK_BORDER: &str = "\u{258c} ";
/// Rows shown for a folded card before the expand hint.
const COLLAPSED_PEEK: usize = 2;

fn card_lines(card: &Card, width: usize) -> Vec<Line<'static>> {
    let (border, body_style) = if card.title == "Assistant" {
        (BLUE, Style::default().fg(TEXT))
    } else {
        match card.kind {
            CardKind::User => (BLUE, Style::default().fg(TEXT)),
            CardKind::Tool => (MUTED, Style::default().fg(MUTED)),
            CardKind::Output => (MUTED, Style::default().fg(MUTED)),
            CardKind::Error => (RED, Style::default().fg(RED)),
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

    // First line carries the role label, opencode-style ("Bash: cmd", user just speaks).
    let total_lines = card.body.lines().count();
    if card.collapsed {
        for source in card.body.lines().take(COLLAPSED_PEEK) {
            for row in wrap_display(source, width.saturating_sub(4)) {
                push_bordered(&mut out, vec![Span::styled(row, body_style)]);
            }
        }
        push_bordered(
            &mut out,
            vec![Span::styled(
                format!(
                    "\u{2026} {} more line(s) \u{b7} /expand",
                    total_lines.saturating_sub(COLLAPSED_PEEK)
                ),
                Style::default().fg(GREEN),
            )],
        );
        return out;
    }

    match card.kind {
        CardKind::User => {
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

    #[test]
    fn wrapping_uses_display_width() {
        let rows = wrap_display("abc界def", 5);
        assert_eq!(rows, vec!["abc界", "def"]);
    }

    #[test]
    fn user_cards_use_thick_left_border() {
        let card = Card {
            kind: CardKind::User,
            title: "User".into(),
            body: "hello".into(),
            collapsed: false,
        };
        let lines = card_lines(&card, 20);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "\u{258c} ");
        assert_eq!(lines[0].spans[0].style.fg, Some(BLUE));
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
            kind: CardKind::Output,
            title: "Tool Output".into(),
            body: (1..=15)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
            collapsed: true,
        };
        let lines = card_lines(&card, 60);
        assert_eq!(lines.len(), COLLAPSED_PEEK + 1);
        let last = lines.last().unwrap();
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("13 more line(s)"));
        assert!(text.contains("/expand"));
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
    /// multi-line notices are split into separate Lines in transcript_text.
    #[test]
    fn line_styled_strips_embedded_newlines() {
        let line = Line::styled("a\nb\nc".to_string(), Style::default());
        let txt: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(txt.matches('\n').count(), 0);
    }

    #[test]
    fn multiline_notices_become_separate_lines() {
        let model = Model {
            notices: vec!["line one\nline two".to_string()],
            ..Model::default()
        };
        let text = transcript_text(&model, 80);
        let rendered: Vec<String> = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
            .collect();
        assert!(rendered.contains(&"line one".to_string()));
        assert!(rendered.contains(&"line two".to_string()));
    }
}
