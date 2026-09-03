# Interactive TUI Architecture

The fullscreen TUI (`src/ui.rs`) follows opencode's design system: near-black surfaces
(`#0a0a0a`/`#141414`/`#1e1e1e`), `#eeeeee` text, `#808080` muted meta, `#484848` borders, and
Kamui blue `#5c9cf5` as the brand accent. Plain mode (`-p`, pipes, `NO_COLOR`, non-TTY) keeps the
line-oriented renderer and never touches the alternate screen.

## Layout

```
┌ transcript (thick-border message rails) ─┐ ┌ sidebar ┐
│                                          │ │ Session │
└──────────────────────────────────────────┘ │ Id      │
[ slash-command menu overlay ]               │ Model   │
┌ editor (❯ input, live cursor) ───────────┐ │ Project │
│ meta: version · model · path             │ │ Context │
└──────────────────────────────────────────┘ │ Last turn│
? help · token badge · queued/scroll hints   └─────────┘
```

- **Home screen** — until the first message, the body shows the two-tone block-letter KAMUI
  logo (muted left half, bright blue right half) with startup notices below.
- **Transcript** — every message is a thick left border (`▌`) with no background fill: user =
  secondary blue, assistant = brand blue with Markdown styling, tool calls muted, errors red.
  Tool output folds by default to a two-line peek with a green `/expand` hint; `/expand` and
  `/collapse` toggle the latest card.
- **Sidebar** — Session title, Id, Model, Project, Context pressure, and the Last turn metrics,
  one per line. Hidden under 84 columns and during the intro screen.
- **Editor** — left-accent bordered box holding the live buffer with a real terminal cursor.
  Enter submits, `\` + Enter continues on a new line (the box grows up to six lines), and the
  meta line shows version/model/path.
- **Slash menu** — appears above the editor while typing `/`; an eight-row sliding window with
  an n/total counter. Tab accepts, arrows navigate, Enter picks or submits.
- **Footer** — one quiet hint line plus the token badge (amber at ≥ 80 % context) and queued
  count while the agent runs.

## InputHub

A single persistent keyboard thread owns all TUI input for the session (`InputHub::spawn`). The
chat loop consumes `HubEvent::{Line, Quit}` from an mpsc channel while a shared `Arc<Mutex<
FullScreen>>` lets the thread redraw the editor on every keystroke.

- While the agent runs, the editor stays live: Enter queues the message (footer shows the count),
  and queued lines run in order when the turn finishes.
- Esc (or Ctrl+C while busy) raises an interrupt through a `tokio::sync::Notify` raced inside
  every await point: request, stream events, tool dispatch, plan approval, and ask_user.
- Approval ([y/N/a]) and ask_user answers travel through one-shot requester channels so they can
  never collide with the editor or the queue.
- Ctrl+C at an idle prompt must be pressed twice within three seconds to quit.

Dialogs (Ctrl+K models, Ctrl+S sessions, `?` help, bare `/model`, `/sessions`, `/help`) are modal
overlays that own the keyboard while open and submit existing slash commands through the same
pipeline, so busy-queue and approval semantics apply unchanged.

## Rendering constraints

- ratatui 0.30 strips embedded newlines from Span content at construction: multi-line notices are
  split into separate Lines before building any Text (see `transcript_text`).
- Wrapping is done by Kamui (`wrap_spans`, word-aware greedy with unicode widths) instead of
  Paragraph's post-scroll wrapping, so bottom-follow viewport math stays exact.
- Raw `println!` anywhere in the TUI path would print into the frame; every command printer
  appends to an output buffer flushed through `ChatUi::notice` (plain mode prints directly).
- `FullScreen` owns raw mode, the alternate screen, and mouse capture; Drop plus a panic hook
  always restore the terminal.
