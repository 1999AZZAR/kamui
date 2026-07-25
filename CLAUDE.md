# Kamui Development Guide

This file is the engineering handoff and working agreement for AI agents contributing to Kamui.
Read `README.md` for user-facing documentation and `ROADMAP.md` for prioritized product phases.

## Product Direction

Kamui is a provider-agnostic LLM CLI written in Rust. It is evolving from interactive chat into a
repository-aware coding agent in the direction of Codex and Claude Code.

The near-term goal is not to build every possible AI feature. Prioritize a reliable coding workflow:

1. Safe read-only repository context.
2. A provider-agnostic tool-call protocol.
3. Permissioned file editing and command execution.
4. Efficient context management and additional providers.

Prefer small, complete capabilities over broad but incomplete systems. Challenge roadmap items whose
effort or operational risk is disproportionate to their immediate value.

## Current Product Behavior

- Every normal launch starts a new chat. Resume must be explicit with `/resume <id>` or
  `kamui -r <id>`.
- `kamui -p <prompt> [--auto-approve]` runs one prompt non-interactively through the same agent
  loop as interactive chat (including tools) and exits. The exchange is persisted as a resumable
  session exactly like an interactive turn, including a generated title. There is no REPL loop, no
  stdin reader, and no spinner. Tool calls that require confirmation are denied by default (the
  model is told the refusal is due to non-interactive mode); `--auto-approve` runs them without
  prompting instead.
- Sessions are created lazily after the first successful streamed response. Empty chats are not
  persisted or listed.
- Completed user/assistant exchanges are stored in SQLite. Partial responses from interrupted or
  failed streams are not added to history.
- The first completed exchange receives an AI-generated title. Title-generation usage is recorded
  with kind `title`, while the request count shown to users counts only primary chat requests.
- Each chat usage row records the model that produced it (`user_version = 5`). `/stats` shows the
  session totals and, when a session used more than one model, a per-model token breakdown.
- Streaming deltas are printed immediately. Usage and finish reason are shown after completion. A
  braille spinner animates from when each request is sent until the first token arrives, then erases
  itself so the response starts on a clean line.
- `Ctrl+C` while a turn is in flight (waiting for the model, streaming, at an approval prompt, or
  running a command) interrupts that turn and returns to the prompt; the partial turn is discarded
  and not saved, and an interrupted command is killed via `kill_on_drop`. `Ctrl+C` at the idle
  prompt shuts down gracefully. Windows stdin uses a reader thread and Tokio channel so the async
  runtime does not block on terminal input.
- Supported chat commands are `/help`, `/new`, `/sessions`, `/resume <id>`, `/model [name]`,
  `/rename <id> <title>`, `/search <text>`, `/compact`, `/undo`, `/jobs`, `/index`, `/delete <id>`,
  `/stats`, `/memory`, `/forget <text>` (or `/forget all`), and `/exit`. Plain `exit` also quits.
- Long sessions are compacted automatically: when the un-summarized recent history exceeds a byte
  threshold (about half the profile's `context_window`, or a default), older messages are folded
  into a rolling summary and the request sends the summary plus recent messages. `/compact` forces
  it. Full history stays in storage; the summary is in-memory and regenerated after a resume.
- `/model` lists the configured provider profiles and marks the active one; `/model <name>` switches
  the active provider and model, rebuilding the provider and persisting the choice in the SQLite
  `settings` table so it survives restarts. The banner shows the active model and profile.
- After each streamed response the usage line reports time-to-first-token and total response time.
  These latency figures are displayed only, not persisted.
- Chat requests offer the model read-only `read_file`, `list_directory`, `grep`, `glob`, and
  `update_plan` tools, `command_status`/`stop_command` for background jobs, plus a
  confirmation-gated `run_command` tool. When the model calls one, Kamui runs a bounded streaming
  agent loop: it executes the tool, prints a one-line trace (or, for `update_plan`, a rendered
  `[ ]`/`[~]`/`[x]` checklist), feeds the result back, and continues until the model returns a
  plain answer. The whole turn is persisted, including the tool requests and results, so resumed
  sessions replay them.
- `run_command` never runs unattended by default. Kamui prints the requested command and prompts
  `[y/N/a]`; `y`/`yes` approves once, declining feeds a refusal back to the model. Three ways to
  skip the prompt: `a`/`always` — ported from the sibling Kumo project's "Always allow" button,
  adapted from Telegram inline buttons to a plain third answer — both approves that call and grants
  the *tool* (not that specific command) a standing pass in `start_chat`'s `always_allowed:
  HashSet<String>` for the rest of the active session, cleared on `/new` or deleting the active
  session (`handle_command` takes `&mut HashSet<String>` for this); a global-only `[permissions]
  allow_commands = [...]` exact-match allowlist (`Tool::requires_confirmation_for`, checked instead
  of the no-arg `requires_confirmation` at dispatch time), configured ahead of time rather than
  granted from the prompt; or launching with `--auto-approve` (accepted by the bare `kamui` command
  too, not just `-p`), which prints a startup banner and skips every confirmation prompt —
  including `[permissions]`-unlisted commands and `patch_file` — for the whole session. Commands run
  in the project directory with stdin disabled, a configurable kill timeout
  (`[commands].timeout_secs`, default 30 seconds), and a 16 KiB output cap, and the result includes
  the exit code with captured stdout and stderr.
- `run_command(background: true)` starts a job without waiting for it: a `tokio::task`
  (`run_background_job`) owns the child process, drains its stdout/stderr independently so a killed
  or timed-out job still reports whatever it produced, and races `child.wait()` against a
  `[commands].background_max_secs` safety timer (default 30 minutes — a backstop against a runaway
  process, not a limit on legitimate long-running commands) and a `tokio::sync::watch` kill signal.
  `command_status` (omit `job_id` to list every job) and `stop_command` read/signal the shared
  in-memory `JobRegistry`; `/jobs` reads it directly without going through the model. Jobs do not
  survive a restart and are killed on shutdown (`tools::kill_all_jobs`, called from `chat::shutdown`
  and the end of `run_once`) so nothing outlives the process.
- When the external `rtk` binary is available (detected once per process), simple approved commands
  are prefixed with `rtk` to compress their output. Commands with shell operators, commands already
  prefixed with `rtk`, and all commands on systems without RTK run directly. The first result line
  records the exact command line executed.
- MCP servers declared as `[mcp.<name>]` in the global config are launched at startup over stdio;
  each advertised tool joins the registry as `<server>__<tool>` and requires approval per call unless
  the server sets `trusted = true`. A server that fails to start is reported and skipped. Project
  configs may not declare servers, since launching one is arbitrary code execution. Only stdio
  transport and the tools capability are supported.
- `patch_file` edits one file per call by exact-match replacement and shows a +/- preview before
  the same `y`/`yes` approval. `old_text` must match exactly once or the patch is rejected with
  recovery guidance; empty `old_text` creates a new file that must not exist. Matching is
  line-ending-agnostic (CRLF files are compared in LF space and rewritten with their original
  endings), so LF `old_text` still matches a CRLF file. Writes are atomic (temp file plus rename)
  and paths pass the same containment checks as reads.
- Approval stays per-file, but the chat loop snapshots each `patch_file` target's pre-turn content
  the first time it is touched in a turn (`chat::snapshot_patch_target`). If the turn is cancelled
  with `Ctrl+C` before it finishes, every file it already changed is reverted automatically
  (`chat::revert_on_cancel`/`revert_snapshot`) so a multi-file edit can never be left half-applied
  with no trace in session history. `/undo` performs the same revert for the most recently
  *completed* turn — one level, in-memory only (not persisted to SQLite), cleared after use or when
  a new turn starts.
- Session IDs may be resolved from an unambiguous prefix. The UI normally displays the first eight
  characters.
- Resume displays the six most recent messages and reports how many earlier messages were omitted.
- Context percentage is displayed only when `KAMUI_CONTEXT_WINDOW` is configured.
- `ask_user` lets the model pause a turn to ask a clarifying question (with up to 4 optional
  numbered choices) instead of guessing. In interactive chat it reads one line from the same
  stdin channel used for command input and approval answers; a numbered reply resolves to that
  option's text, anything else is returned as typed. In `-p` non-interactive mode there is no user
  to ask, so it is declined with a message telling the model to proceed on its own judgment. It is
  intercepted by the chat loop before `ToolRegistry::dispatch`, since it needs the interactive
  stdin channel that `Tool::run` has no way to receive.
- `spawn_agent` delegates a self-contained task to an isolated sub-agent (`chat::run_spawned_agent`)
  and returns only its final text, so the sub-agent's own tool trace never enters the parent
  conversation. Intercepted the same way `ask_user` is, since it needs the active `Provider`/model
  and `ProjectContext` directly. The sub-agent gets a fresh system prompt, no shared history with
  the parent, and runs against `ToolRegistry::read_only` (`read_file`/`list_directory`/`grep`/
  `glob` only — no `run_command`, `patch_file`, memory tools, or recursive `spawn_agent`), so none
  of its tool calls ever need confirmation and it cannot mutate anything. It runs sequentially and
  blocks the parent turn; concurrent sub-agents are not supported, since the interactive approval
  prompt and stdin channel are single-consumer.
- `/index` rebuilds the semantic-search index (`chat::run_index`): walks the project the same
  `.gitignore`-aware way `grep`/`glob` do, splits each file into fixed 50-line chunks
  (`tools::chunk_text`), skips any file whose content hash (`chat::content_hash`, a non-cryptographic
  change-detection hash) matches what was stored last run, embeds the rest via the active profile's
  `Provider::embed`, and removes chunks for files no longer present. Requires
  `Profile::embedding_model` to be set; otherwise it fails with a clear message and `search_code` is
  never offered to the model. `search_code` (also intercepted like `ask_user`, since it needs
  `Provider`/`Database`) embeds the query and ranks every stored chunk by cosine similarity
  (`chat::cosine_similarity`), a brute-force scan with no vector index — see "Storage Decisions" and
  README for the v1 scope this accepts.
- `remember`, `update_memory`, and `forget` manage a global memory table (`user_version = 6`),
  intercepted the same way `ask_user` is (they need `Database` directly). Unlike project
  instructions (`KAMUI.md`, a file the user edits) or session history (scoped to one conversation),
  a remembered fact is global and permanent: it is read fresh from storage on every turn (not
  frozen at startup, since Kamui is a single interactive process rather than a server shared
  across many chats) and is visible in every project Kamui is run in afterward. `update_memory` and
  `forget` resolve their target via an unambiguous case-insensitive substring match, failing with
  guidance on zero or multiple matches rather than guessing. Total stored memory is capped at 4
  KiB; `/memory` lists what is remembered and `/forget <text>` (or `/forget all`) removes it
  directly, without going through the model.
- `kamui doctor` checks configuration, provider connectivity (a real test request), and MCP
  server connections one at a time with pass/fail output, exiting non-zero if anything failed —
  usable as a pre-flight check without starting a full chat session.
- `kamui status` (`main::run_status`) prints a config/database summary with no network calls at
  all — profile, model, base URL, all configured profiles, `embedding_model`, MCP server names,
  the `[permissions]` allowlist, project and database paths, session count, memory count, and
  indexed-chunk count. Ported from the sibling Kumo project's `kumo status` (read-only summary,
  distinct from `doctor`'s active checks), adapted from Kumo's single-workspace/single-provider
  config to Kamui's per-project root and multi-profile config.

## Repository Context

The process working directory is the project root.

- Every request begins with an agentic system prompt (`src/prompt.rs`) that tells the model how to
  work as a terminal coding agent; its tool-usage guidance is included only when the active profile
  offers tools. At startup, Kamui loads `KAMUI.md` if present, otherwise `AGENTS.md`, and appends
  that content to the system prompt.
- `CLAUDE.md` is an agent development guide and is intentionally not loaded by Kamui at runtime.
- A prompt can attach UTF-8 text with a relative reference such as `@src/main.rs`.
- Referencing a directory (`@src`) attaches the text files inside it, walked with the `ignore` crate
  so `.gitignore`, global excludes, and hidden files are honoured (`require_git(false)`, so rules
  apply outside a repository too). Attachment stops at the shared context budget or 50 files;
  leftovers are reported in a trailing note rather than failing the prompt.
- Each file is limited to 64 KiB and all attached context is limited to 128 KiB per request.
- Absolute paths, directories, binary/non-UTF-8 files, and paths or symlinks outside the project root
  are rejected.
- Duplicate references are attached once.
- Expanded file contents are sent for that request only. The original user prompt, not expanded
  contents, is stored in session history.
- `@diff` attaches raw unstaged tracked changes using `git diff`.
- `@staged` attaches raw staged changes using `git diff --cached`.
- `@clipboard` attaches the system clipboard (via `arboard`): text when present, otherwise clipboard
  image data encoded as PNG, so a screenshot can be pasted without saving a file. It errors clearly
  when the clipboard is unavailable (headless) or holds neither. Reading the real clipboard is
  environment-dependent and has no unit test; the PNG encoding is tested directly.
- An `@` reference ending in `.png`, `.jpg`, `.jpeg`, `.gif`, or `.webp` is attached as an image
  (base64 `ImageAttachment` on the message), not inlined as text. Images pass the same containment
  checks, are capped at 5 MiB each, are sent only for that request, and require a vision model. The
  OpenAI adapter switches message content to a parts array only when images are present.
- Untracked files are not in `@diff`; users must attach them explicitly with `@path`.
- Raw diff is deliberate. Do not silently replace it with a condensed representation because code
  review may require details omitted by summarization.

## Architecture

Important modules:

- `src/main.rs`: CLI argument parsing, configuration loading, dependency construction, and startup.
- `src/config.rs`: `kamui.toml` discovery, global/project layering, named provider profiles, and the
  first-run scaffold.
- `src/prompt.rs`: the agentic system prompt, combined with project instructions per request.
- `src/compaction.rs`: rolling-summary context compaction (threshold, message selection, summary
  request); the chat loop drives it automatically and via `/compact`.
- `src/mcp.rs`: MCP client over stdio via the `rmcp` SDK; wraps each server tool as a Kamui `Tool`.
- `src/chat.rs`: interactive loop, streaming display, session commands, title generation, the
  streaming tool agent loop, graceful shutdown, and `run_once` for non-interactive `-p` prompts.
- `src/context.rs`: project instruction discovery and safe `@file`, `@diff`, and `@staged`
  expansion, including the shared `read_project_file` path-safety helper.
- `src/tools.rs`: the async `Tool` trait, `ToolRegistry` dispatch, the read-only `read_file`,
  `list_directory`, `grep`, `glob`, `update_plan`, `command_status`, and `stop_command` tools, and
  the confirmation-gated `run_command` and `patch_file` tools. `run_command`'s background-job
  registry (`JobRegistry`, `JobEntry`, `run_background_job`) also lives here, as does
  `ToolRegistry::read_only` (the restricted registry `spawn_agent`'s sub-loop runs against) and
  `chunk_text`/`search_code_definition` (the fixed-size chunker and conditionally-offered
  `search_code` definition that `chat::run_index`/`chat::dispatch_search_code` use).
- `src/provider/mod.rs`: provider-independent request, response, message, usage, and streaming types.
- `src/provider/openai.rs`: OpenAI-compatible Chat Completions HTTP and SSE implementation.
- `src/storage.rs`: SQLite schema, migration, sessions, messages, usage, persistence tests, and the
  `/index` semantic-search store (`code_chunks`/`indexed_files`, `CodeChunk`).
- `.github/workflows/release.yml`: tag-triggered multi-platform release builds.
- `install.ps1` and `install.sh`: release-binary installers with SHA-256 verification.

Keep the core provider-agnostic. Provider-specific payloads and parsing belong under the provider
implementation. Do not leak OpenAI response structures into chat, storage, context, or future tool
runtime APIs.

The current `Provider` trait supports non-streaming `chat` and streaming `chat_stream`, plus `embed`
(a batch text-to-vector call for `/index`/`search_code`, `src/provider/openai.rs` via the
OpenAI-compatible `/embeddings` endpoint). The non-streaming `chat` path is used for title
generation and is the intended path for tool-calling turns. `embed` is only ever called when the
active profile's `Profile::embedding_model` is set.

The provider-agnostic tool-call protocol is modeled in `provider/mod.rs` as `ToolDefinition`,
`ToolCall`, and tool-request/tool-result `Message` variants; `ChatRequest` carries `tools` and both
`ChatResponse` and `StreamEvent::Done` surface `tool_calls`. The OpenAI adapter maps these to and
from wire types entirely within `provider/openai.rs`, including index-keyed reassembly of tool calls
streamed across deltas, so the core no longer serializes its own types into an OpenAI-shaped payload.
Native Anthropic and Gemini adapters must reuse these same neutral types.

Tools live in `src/tools.rs`. `ToolRegistry` holds boxed async `Tool` implementations, exposes their
`ToolDefinition`s, and dispatches a `ToolCall` by name, returning any failure as an `Error: ...`
string so the model can recover rather than aborting the turn. Read-only `read_file`,
`list_directory`, `grep`, and `glob` reuse `context::resolve_within_root` for path safety and the
`ignore` crate's `.gitignore`-aware walking (also used by `@dir` expansion); `run_command` executes
shell commands with a timeout and output cap. Permission policy stays in Kamui, not the tools: a tool
advertises `requires_confirmation`, and the chat loop prompts the user before dispatching any such
call, feeding a refusal back to the model if declined. The chat loop runs a streaming agent loop
bounded by `MAX_TOOL_ROUNDS`: it streams a turn, and if the model requested tools it records the
request, runs each tool, appends the results, and requests again until a plain answer arrives.

Whole turns are persisted, including tool messages. A `user_version = 3` migration rebuilds the
`messages` table to allow the `'tool'` role and store `tool_calls` (JSON) and `tool_call_id`;
`save_turn` writes the prompt, tool requests, tool results, and final answer atomically, so resumed
sessions replay tool interactions. Recorded usage is still the final round's, not the whole turn's.
The terminal runner, mutation tools, per-turn usage accounting, and a durable audit trail remain.

## Storage Decisions

- SQLite is compiled with the `bundled` feature so end users do not install SQLite separately.
- Use schema migrations through `PRAGMA user_version`; do not make destructive schema assumptions.
- The default database is in the operating system local application data directory under `kamui`.
- `KAMUI_DATA_DIR` overrides the data directory for servers and containers.
- Multi-device synchronization, if built, should exchange records through an API. Do not synchronize
  by copying a live SQLite database.
- Foreign keys and cascading session deletion must remain enabled.
- Save an exchange and its usage atomically.
- `code_chunks`/`indexed_files` (`user_version = 7`) are a known, accepted exception to "one global
  database": `path` is project-relative, not project-scoped, so indexing two different projects
  into the same database would collide. Fixing this (e.g. a project-id column) is future work if it
  becomes a real problem; not worth the complexity for a single-project v1.

## Configuration

Provider and model settings come from `kamui.toml` files; `src/config.rs` owns loading. No
environment variables participate in provider or model configuration — `.env`/dotenvy support was
removed. Precedence is:

1. Project `kamui.toml` in the working directory (non-secret only).
2. Global `kamui.toml` in the OS configuration directory (may hold the API key).
3. Built-in defaults.

Two forms are accepted. The flat form sets top-level `model`/`context_window` and a `[provider]`
section. The multi-profile form defines named `[profiles.<name>]` entries (each with `model` and
optional `base_url`/`api_key`/`context_window`/`tools`) plus a `default_profile`; profiles win when
present, and `/model` switches between them at runtime with the active choice stored in the SQLite
`settings` table. A single implicit profile named `default` backs the flat form. A profile may set
`provider = "<name>"` to inherit `base_url`/`api_key`/`tools` from a shared `[providers.<name>]`
block, so one key can back many model profiles; inline profile fields override the shared block.

Fields:

- `model`: required model identifier (no default).
- `context_window`: optional integer used for context-percentage reporting.
- `[provider].base_url` / `[profiles.*].base_url`: OpenAI-compatible base URL, defaults to
  `https://api.openai.com/v1`.
- `[provider].api_key` / `[profiles.*].api_key`: required; allowed only in the global file. A
  project file that sets it anywhere is rejected, so a committed project config can never leak a key.
- `default_profile`: chooses the starting profile when several are defined; a project file may set
  it to pin a project to a profile.
- `tools` (under `[provider]` or `[profiles.*]`): whether to offer tools to that model, default
  true. Set false for endpoints/models that reject the `tools` field (many small local models) so
  plain chat still works; `/model` marks such profiles `[no tools]`.
- `[permissions].allow_commands`: exact-match `run_command` commands that skip approval, global-only
  like `api_key` — a project file that defines `[permissions]` at all is rejected, since an
  allowlisted command grants unattended execution the same way a leaked key would.
- `[commands].timeout_secs` / `[commands].background_max_secs`: `run_command`'s foreground timeout
  (default 30) and the safety cap on a `background: true` job's lifetime (default 1800). Not
  security-relevant, so unlike `[permissions]` a project file may override either.
- `[provider].embedding_model` / `[profiles.*].embedding_model`: an embedding-capable model on the
  same provider, enabling `/index` and `search_code`. Inherits from a shared `[providers.*]` block
  the same way `base_url`/`api_key`/`tools` do. `None` (the default) leaves semantic search
  unavailable — `search_code` is then not offered to the model at all, rather than erroring.

On first run, when no global `kamui.toml` exists, Kamui scaffolds the global config directory with a
commented template and exits, asking the user to fill in the key. `KAMUI_DATA_DIR` remains an
environment override for the database location only (a container/ops concern, not provider config).

Never commit API keys, credentials, provider responses containing secrets, or local database files.
A project `kamui.toml` is safe to commit because it cannot contain a key. If a key appears in logs,
chat, commits, or screenshots, advise immediate rotation.

OpenRouter, Ollama, LM Studio, Groq, DeepSeek, and LiteLLM work through an OpenAI-compatible base
URL by editing `provider.base_url` and `model` in `kamui.toml`; README documents OpenRouter and
Ollama examples. Describe these as OpenAI-compatible services, not native provider integrations.
Native Anthropic and Gemini support would require dedicated adapters and is not currently planned.

## RTK Decision

[RTK](https://github.com/rtk-ai/rtk) is an optional external execution backend, now wired into
`run_command`: it is detected once per process and simple commands are prefixed with `rtk`, while
anything with shell operators, an existing `rtk` prefix, or a system without RTK runs directly.
It is a Rust application, but it currently exposes a binary target rather than a stable public Rust
library API. Do not add it as a Cargo dependency or copy its source into Kamui.

The intended execution flow is:

```text
model requests a command
  -> Kamui validates permission and policy
  -> Kamui applies timeout, cancellation, and output limits
  -> supported command runs through the external `rtk` binary when available
  -> otherwise command runs directly
  -> Kamui records the command, exit status, and result
  -> compact output enters model context
```

RTK responsibilities:

- Filter, group, deduplicate, and compress command output.
- Preserve useful failures and command exit status.
- Reduce model context used by tests, builds, searches, Git, containers, and other supported tools.

Kamui responsibilities that RTK does not replace:

- Tool-call protocol.
- User permission and confirmation policy.
- Path and command safety.
- Timeout and cancellation.
- Output size limits.
- Audit trail and recovery behavior.
- Direct-command fallback.

Do not require RTK for normal chat or repository context. Detect it at runtime. A later `kamui doctor`
command may report its availability and version, and installers may offer it as an optional install.

## Priorities

The source of truth is `ROADMAP.md`. Current priority order is:

1. Phase 2 is complete. Custom global instructions and Markdown export were descoped; do not start
   them without a concrete user request.
2. Phase 3 is complete and has grown past its original scope: the provider-agnostic tool-call
   protocol, streaming agent loop, read/search tools (`read_file`/`list_directory`/`grep`/`glob`),
   the confirmation-gated command runner (configurable timeout, optional RTK routing, a global
   `[permissions]` allowlist, `--auto-approve`, and `background: true` jobs with
   `command_status`/`stop_command`/`/jobs`), the confirmation-gated `patch_file` editor with a
   turn-scoped snapshot/auto-revert safety net and `/undo`, `update_plan` for a live checklist,
   `spawn_agent` for a narrow (sequential, read-only) sub-agent, whole-turn persistence including
   tool messages, per-turn usage accounting, and interrupt-and-continue cancellation. Multi-file
   editing is repeated `patch_file` calls within a turn; Git works through `run_command` plus
   `@diff`/`@staged`; the audit trail is the persisted tool messages.
3. Phase 5 is complete for its planned scope (config, runtime `/model` switching with profiles and
   shared credentials, OpenAI-compatible docs, per-model stats). An agentic system prompt
   (`src/prompt.rs`) now ships on every request.
4. Phase 4 context management: image input, directory context, and context compaction are done.
   Semantic search (`/index`/`search_code`) shipped as a deliberately simple v1 — fixed-size line
   chunking, brute-force cosine similarity, no vector index — rather than the larger effort
   originally deferred; project indexing *at a larger scale* (richer chunking, a real vector index)
   remains open future work if the v1 approach stops scaling. Excel/PDF input are handled via MCP;
   Anthropic/Gemini native providers are not planned.

Avoid starting these early because their true scope is large:

- Project indexing at scale (a real vector index, syntax-aware chunking) beyond the v1 semantic
  search already shipped.
- Further context compression beyond the rolling-summary compaction already shipped.
- Plugin systems, remote workers, or a general-purpose background job queue (`run_command`'s own
  background jobs and `spawn_agent`'s narrow sequential sub-agent are already done — see above).
- GUI, mobile, and voice clients.

## Coding Principles

- Prefer the smallest correct change.
- Keep behavior cross-platform across Windows, Linux, and macOS.
- Treat filesystem boundaries, symlinks, command invocation, and subprocess output as hostile input.
- Preserve existing user data and shipped behavior when changing storage or sessions.
- Avoid backward-compatibility layers unless persisted data, released behavior, or external consumers
  require them.
- Add concise comments only where behavior is not self-explanatory.
- Keep user-facing command names consistent. Resume uses `/resume` and `-r`; do not introduce
  ambiguous aliases without a concrete need.
- Do not persist expanded repository context unless a future design explicitly requires it.
- Do not count title-generation calls as primary chat requests.
- Do not save a partially streamed exchange as if it completed successfully.

## Verification

Before considering Rust changes complete, run:

```sh
rtk cargo fmt --all
rtk cargo test
rtk cargo clippy --all-targets --all-features -- -D warnings
rtk git diff --check
```

If RTK is unavailable, run the same commands without the `rtk` prefix. Also run `cargo check` or a
release build when changing dependencies, platform behavior, installers, or release packaging.

Current tests cover persistence, cascade deletion, session summaries, hidden empty sessions, SSE
parsing, project instruction precedence, file-reference expansion, duplicate references, unchanged
plain prompts, and staged Git diff expansion. Add focused tests for new parsing, storage, safety, and
cross-platform path behavior.

## Git and Releases

- Do not commit or push unless the user explicitly requests it.
- Do not rewrite or move a published tag. If release code changes after a tag, create a new patch
  version and tag.
- Release tags matching `v*` trigger five build targets: Windows x64, Linux x64, Linux ARM64, macOS
  Intel, and macOS Apple Silicon.
- GitHub Release assets are required before public installers work because installers download from
  `releases/latest/download` and verify checksums.
- Tag `v0.1.0` points to the initial persistent streaming chat release commit. Its workflow run was
  blocked before jobs started because the GitHub account was locked due to a billing issue. No empty
  GitHub Release was intentionally published. Re-run or create the next patch release only after
  GitHub Actions billing is operational.

## Known Limitations

- The current provider uses the Chat Completions API, not a native Responses API or native tool-call
  loop.
- Paths containing spaces cannot currently be represented by the whitespace-based `@file` parser.
- Project instructions are loaded only from the launch directory, not recursively from ancestors or
  nested directories.
- `@diff` excludes untracked files and `@diff`/`@staged` require Git on `PATH`.
- Context limits are byte-based rather than tokenizer-aware.
- Cost analytics are intentionally deferred because pricing metadata and multi-provider semantics are
  not yet defined.
- Unix installer behavior has not been exercised locally from the Windows development environment.

## Definition of Done

A feature is complete when:

- Its behavior is provider-neutral unless explicitly provider-specific.
- Failure modes are clear and do not corrupt sessions or files.
- Relevant unit tests exist.
- Formatting, tests, strict Clippy, and diff checks pass.
- User-facing behavior is documented in `README.md`.
- Product priority or completion state is reflected in `ROADMAP.md`.
- No secret, local `.env`, database, build artifact, or unrelated worktree change is included.
