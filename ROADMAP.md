# Kamui Roadmap

Kamui is evolving from a provider-agnostic chat CLI into a repository-aware coding agent. The
roadmap prioritizes a safe, useful read-only workflow before file mutation and command execution.

Status: Phases 1–5 are complete for their planned scope, and the MCP client has shipped. Kamui is a
working coding agent — it explores, reads, runs commands (with approval and optional RTK
compression), and edits files (with a diff preview and approval), persisting whole turns and letting
the user interrupt with `Ctrl+C`. Long sessions compact themselves into a rolling summary. It is
configured through `kamui.toml`, `/model` switches between named provider profiles at runtime, and
MCP servers can contribute their own tools.

What remains is deliberately deferred: project indexing and semantic search (large, separate
efforts), the Phase 6 terminal experience (markdown rendering, syntax highlighting, richer UI), and
the extensibility items below. Several roadmap entries are marked "not planned" where a simpler
answer already exists.

## Phase 1: Chat Foundation

- [x] Interactive streaming chat
- [x] Provider-agnostic core
- [x] OpenAI-compatible provider
- [x] SQLite session storage
- [x] Session list, resume, and delete
- [x] Auto title generation
- [x] Token and context usage statistics
- [x] Cross-platform installers and release workflow

## Phase 2: Repository-Aware Chat

- [x] Project instructions (`KAMUI.md` or `AGENTS.md`)
- [x] Read-only `@file` context
- [x] Git diff context (`@diff` and `@staged`)
- [x] Session rename
- [x] Conversation search
- [x] Latency and time-to-first-token tracking

Descoped from Phase 2 (not planned): custom global instructions and Markdown export. Do not
re-add without a concrete user request.

## Phase 3: Coding Agent Runtime

- [x] Provider-agnostic tool-call protocol
- [x] Tool runtime, dispatch, and streaming agent loop
- [x] Read file tool
- [x] List directory tool
- [x] Safe terminal command runner with permission, timeout, and output limits
- [x] Background command execution (`run_command(background: true)`, `command_status`,
  `stop_command`, `/jobs`)
- [x] Preserve raw output on failures and command exit codes
- [x] Optional RTK execution backend with direct-command fallback
- [x] Patch file tool with confirmation
- [x] Multi-file editing
- [x] Git status, diff, and commit integration
- [x] Tool audit trail
- [x] Cancellation and failed-tool recovery

Phase 3 is complete. Kamui can explore (`list_directory`), read (`read_file`), execute
(`run_command` with approval and optional RTK compression), and edit (`patch_file` with preview and
approval), with whole turns persisted so resumed sessions replay tool interactions. A turn's
recorded usage folds every agent-loop round together: output tokens accumulate across rounds while
the input count tracks the final round, so context reporting stays correct without double-counting.

How the last four items are satisfied:

- Multi-file editing: the agent loop runs any number of `patch_file` calls within a single turn,
  each independently previewed and approved, so a change spanning several files is one conversation
  turn. A single atomic multi-file transaction is deliberately not built; each file is its own
  reviewable, atomic write.
- Git status, diff, and commit integration: Git is available through `run_command` (with approval
  and RTK compression) for status, diff, and commit, and read-only history context is available
  through `@diff` and `@staged`. No Git-specific tool is needed on top of these.
- Tool audit trail: every tool request and result is persisted as part of the turn (see the tool
  message persistence above) and replayed on resume, which is the durable record of what ran.
- Cancellation and failed-tool recovery: `Ctrl+C` during a turn interrupts it and returns to the
  prompt, killing any running command, without saving the partial turn; at the idle prompt it exits.
  Tool failures (bad arguments, a patch that does not match, a declined command, an unknown tool)
  are returned to the model as text so it can recover on the next round rather than aborting.

Progress: the provider-agnostic tool-call types (`ToolDefinition`, `ToolCall`, tool-request and
tool-result messages), their OpenAI serialization, and both non-streaming and streaming (index-keyed
delta reassembly) parsing have landed with tests. The core no longer serializes its own message
types into an OpenAI-shaped payload; wire mapping lives in the provider. A `ToolRegistry` dispatches
calls, read-only `read_file` and `list_directory` tools reuse the shared `@file` path-safety checks,
and the chat loop runs a streaming agent loop (bounded by a per-turn round limit) that executes
requested tools and feeds results back until the model returns a plain answer. Tool failures are
returned to the model as text so it can recover.

The `run_command` tool executes shell commands in the project directory. Kamui owns the permission
policy: any tool that reports `requires_confirmation` (`run_command` and `patch_file`) is shown to
the user and must be approved with `y`/`yes` before it runs, unless it matches the global
`[permissions] allow_commands` exact-match allowlist or `--auto-approve` is active; declining feeds
a refusal back to the model. Commands run with stdin disabled, a configurable timeout that kills the
process (`[commands].timeout_secs`, default 30 seconds), and a 16 KiB output cap; the result carries
the exit code plus captured stdout and stderr.

`run_command(background: true)` starts a job without waiting: it returns a job id immediately, a
`tokio::task` drains the process independently (so output survives a kill/timeout), and
`command_status`/`stop_command`/`/jobs` check on or stop it. Jobs are in-memory only, bounded by
`[commands].background_max_secs` (default 30 minutes) as a runaway-process backstop, and are killed
on shutdown so nothing outlives the Kamui process.

RTK routing is in place: the `rtk` binary is detected once per process, and simple commands are
prefixed with `rtk` so compressed output reaches model context. Commands containing shell operators,
commands already prefixed with `rtk`, and every command on systems without RTK run directly. The
first line of each result records the exact command line that was executed.

The `patch_file` tool edits one file per call by exact-match replacement: `old_text` must match the
file exactly once, or the patch is rejected with guidance so the model re-reads and retries; an
empty `old_text` creates a new file that must not already exist. Every patch shows a +/- line
preview and requires the same `y`/`yes` approval as commands. Writes go through a temporary file
and rename so an interrupted write cannot leave a half-written file, and paths resolve through the
same containment checks as reading (a new file's parent directory must already exist inside the
project). Multi-file editing beyond one file per call remains future work.

Tool messages are persisted. A `user_version = 3` migration rebuilds the `messages` table to allow
the `'tool'` role and store `tool_calls` and `tool_call_id`, and a whole turn (prompt, tool requests,
tool results, final answer) is saved atomically, so resumed sessions replay the tool interactions.
Per-turn usage now folds every agent-loop round together (output summed, input from the final round).
`Ctrl+C` interrupts a turn and returns to the prompt without saving it, killing any running command.

RTK is an execution optimization, not the command permission layer. When installed, Kamui should
route supported terminal commands through the external `rtk` binary to reduce tool output before it
enters model context. Unsupported commands and systems without RTK must continue to work directly.
Raw `git diff` remains available for code review because a condensed diff can omit required detail.

## Phase 4: Context Management

- [x] Directory context with ignore rules
- [x] Clipboard context (`@clipboard`, text or image)
- [x] Conversation summarization
- [x] Context compression
- [x] Project indexing (a deliberately simple v1 — see below; a larger-scale version remains open)
- [x] Semantic search (`/index`, `search_code`)
- [x] Image input
- [ ] PDF input (not planned — handled via MCP)

Directory context is built. Referencing a directory (`@src`) attaches the text files inside it,
walking with the `ignore` crate so `.gitignore`, global excludes, and hidden files are honoured even
outside a git repository. Files are attached in path order until the shared 128 KiB context budget
or a 50-file cap runs out; anything left over (binary, too large, or over budget) is reported in a
trailing note instead of failing the prompt.

Image input is built, from a file or the clipboard. Referencing an image (`@shot.png`, also
`.jpg`/`.jpeg`/`.gif`/`.webp`) attaches it to the request instead of inlining bytes as text: the file
is read through the same containment checks, capped at 5 MiB, and carried as a base64
`ImageAttachment` on the message. `@clipboard` prefers text but falls back to clipboard image data,
encoding it as PNG, so a screenshot can be pasted straight into a prompt — terminals cannot accept
pasted image data directly, so this is the paste path. The OpenAI adapter
emits a content-parts array (`text` plus `image_url` data URLs) only when images are present, so
text-only requests keep their plain-string content. Images are per-request and are not persisted.
The model must support vision.

Context compaction is built (`src/compaction.rs`). When a session's un-summarized recent history
grows past a byte threshold (about half the profile's `context_window`, or a default), the older
messages are folded into a rolling summary via one non-streaming request, and the request thereafter
sends the agentic prompt plus that summary plus the most recent messages verbatim. `/compact`
triggers it manually. The full history stays in storage; only the per-request message list is
compressed, and the summary is in-memory (regenerated after a resume). Excel/PDF input are handled
through MCP.

Semantic search shipped as a deliberately simple v1 rather than the larger effort originally
deferred. `Provider` gained an `embed(model, texts) -> Vec<Vec<f32>>` method
(`src/provider/openai.rs`, via the OpenAI-compatible `/embeddings` endpoint), and a profile can set
`embedding_model` to enable it. `/index` (`chat::run_index`) walks the project the same
`.gitignore`-aware way `grep`/`glob` do, splits each file into fixed, non-overlapping 50-line
chunks (`tools::chunk_text` — no syntax awareness), and skips any file whose content hash
(`chat::content_hash`) matches what `indexed_files` (`user_version = 7`) recorded last run, so a
second `/index` after a small edit only re-embeds what changed. Chunks and their embedding vectors
(little-endian `f32` bytes in a BLOB column) live in `code_chunks`. `search_code` embeds the query
and ranks every stored chunk by cosine similarity (`chat::cosine_similarity`) — a brute-force scan,
no vector index — returning the top 8 as `path:start-end` with the chunk text. Without
`embedding_model` configured, `search_code` is not offered to the model at all.

Two accepted v1 limitations, worth revisiting only if they become real problems: chunk boundaries
are fixed-size, not syntax- or semantically-aware, and `code_chunks`/`indexed_files` store a
project-relative `path` with no project-id column, so indexing two different projects into the
same global database would collide.

## Phase 5: Providers and Models

- [x] Structured configuration file (`kamui.toml`) for provider, model, and settings
- [x] Runtime model switching
- [x] Document OpenAI-compatible services (OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM)
- [x] Provider and model statistics
- [ ] Temperature and model parameters (not planned — coding agents keep defaults simple; Codex and
  Claude Code do not expose these either)
- [ ] Provider and model discovery (not planned — profiles are defined manually in `kamui.toml`)
- [ ] Native Anthropic provider (not planned)
- [ ] Native Gemini provider (not planned)

Phase 5 is complete for the planned scope: configuration, runtime model switching with profiles and
shared credentials, OpenAI-compatible provider documentation, and per-model usage statistics all
shipped. A `user_version = 5` migration records the model on each usage row, and `/stats` shows a
per-model breakdown when a session used more than one model. The remaining items are descoped.

Configuration is built (`src/config.rs`): a TOML `kamui.toml` replaces `.env` and `dotenvy`. A
global `kamui.toml` in the OS config directory may hold the API key; a per-project `kamui.toml` in
the working directory is non-secret only and is rejected if it sets `api_key`. Resolution precedence
is project, then global, then built-in defaults, with no environment variables in provider or model
configuration. First run scaffolds the global config directory with a commented template and exits
so the user can fill in the key. `KAMUI_DATA_DIR` still overrides only the database location.

Runtime model switching is built. The config accepts named `[profiles.<name>]` entries; the flat
single-provider form remains valid as one implicit profile. Several profiles can share one key by
referencing a `[providers.<name>]` block. A per-profile `tools` flag (default true) turns tools off
for models that reject the `tools` field — many small local models — so plain chat still works; those
profiles are shown as `[no tools]`. In chat, `/model` lists profiles and `/model <name>` switches the
active provider and model, persisting the choice in the SQLite `settings` table so it survives
restarts. The provider is rebuilt through a factory injected by `main`, so the chat loop stays
provider-neutral.

Thinking spinner and interrupt-and-continue (Phase 6 items) also shipped early: a braille spinner
animates until the first token, and `Ctrl+C` mid-turn returns to the prompt instead of exiting.

## Phase 6: Terminal Experience

- [x] Project status card and `/status` refresh
- [ ] Markdown rendering
- [ ] Syntax highlighting
- [x] Thinking indicator (braille spinner until the first token)
- [ ] Rich terminal UI
- [ ] Session pinning
- [ ] Prompt templates
- [ ] System prompt profiles
- [ ] Daily and monthly usage reports
- [ ] Optional cost tracking
- [ ] Benchmark mode

## Later: Extensibility and Remote Work

- [x] MCP client
- [ ] Plugin API and manager (not planned — see "Plugin decision" below)
- [ ] Background jobs and job queue (a narrower form — `run_command(background: true)` — shipped in
  Phase 3; this item is the broader, general-purpose async job queue, still open)
- [ ] Scheduled tasks
- [ ] Worker nodes and remote execution
- [ ] Local memory and RAG
- [x] Multi-agent workflows (a first, deliberately narrow form: `spawn_agent`)

The MCP client is built (`src/mcp.rs`, via the `rmcp` SDK). Servers declared as `[mcp.<name>]` in the
global `kamui.toml` are launched as child processes over stdio; each tool they advertise is wrapped
as a Kamui `Tool` with a `<server>__<tool>` name, so MCP tools flow through the same registry,
permission policy, and agent loop as the built-ins. Every MCP call asks for approval unless the
server is marked `trusted`. Project files may not declare servers, because launching one is arbitrary
code execution. A server that fails to start is reported and skipped rather than blocking startup.
Only stdio transport and the tools capability are supported; HTTP transport, resources, and prompts
are not.

**Plugin decision**: a dedicated plugin runtime was considered and rejected. Kamui is already an
MCP *client*, so "write a plugin" already means "write (or reuse) an MCP server" — the gap is
packaging/discovery (no `kamui plugin install <name>`), not the extension mechanism. A realistic
native plugin API would mean either unsafe/version-fragile dylib loading, or a WASM sandbox that
converges back on "an MCP server, in our own protocol." Closing MCP's actual gaps (HTTP transport,
resources, prompts) is a better use of effort than a bespoke plugin system.

`spawn_agent` (`src/tools.rs`/`chat::run_spawned_agent`) is Kamui's first multi-agent primitive: it
delegates a self-contained task to an isolated sub-agent — fresh system prompt, no shared history
with the parent — and returns only the sub-agent's final answer, so its own tool-call trace never
enters the parent's context. Deliberately narrow for v1: the sub-agent runs against
`ToolRegistry::read_only` (`read_file`/`list_directory`/`grep`/`glob` only, no `run_command`,
`patch_file`, memory tools, or recursive `spawn_agent`), so none of its calls ever need
confirmation and there is no approval flow to reproduce. It also runs sequentially, blocking the
parent turn — concurrent sub-agents are future work, gated on rethinking how approval prompts and
the single-consumer stdin channel would interleave across multiple agents running at once.

## Not Planned Soon

- [ ] GUI and dashboard
- [ ] Mobile app
- [ ] Voice mode
- [ ] MCP server
- [ ] Infrastructure-specific plugins (Docker, Kubernetes, PostgreSQL)
