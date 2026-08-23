# Interactive transcript UI

The interactive chat path uses `src/ui.rs` as a retained transcript surface. The UI stores cards for user prompts, tool calls, tool output, errors, notices, and the currently streaming assistant response. Each state update redraws a Ratatui frame from the model instead of appending permanent ANSI fragments to the terminal.

The visual contract is intentionally close to the prototype: User cards use blue, Tool cards use slate gray, Tool Output cards use a darker slate, and Error cards use red. Every card has a titled top border and a full-width body. Tool output is still collapsed at the source through the existing head/tail preview policy; `/expand` and `/collapse` toggle the latest retained card.

The assistant path passes raw Markdown through `markdown::render_ratatui`, which produces owned `Line` and `Span` values. Headings and list markers are bold, code spans and fenced code use cyan, and blockquotes or fence markers use a dim color. This keeps formatting semantic and allows the Paragraph widget to wrap content instead of counting ANSI bytes.

The interactive surface is enabled only when the existing TTY detector says the session is interactive. `-p`, redirected output, pipes, and `NO_COLOR` continue using the line-oriented renderer. `FullScreen` owns alternate-screen setup and teardown through its `Drop` implementation, including cursor restoration and `LeaveAlternateScreen`.

The current implementation keeps the provider and tool agent loop intact and introduces a UI seam through `ChatUi`. A future event-bus refactor can replace direct `ChatUi` mutations with `TimelineEvent` messages without changing provider protocol, tool registry, storage, or approval semantics.
