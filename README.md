# kamui

Provider-agnostic, repository-aware coding agent for the terminal, written in Rust.

Kamui explores your repository, reads files, runs commands, and edits code, with every side effect
gated behind your approval. Responses stream from any OpenAI-compatible model, `/model` switches
between providers and models mid-session, long conversations compact themselves so they do not
outgrow the context window, and MCP servers can contribute their own tools.

The core request and response types are independent of any single provider's API, so new providers
can be added without changing the chat interface.

## Configuration

Kamui is configured with a `kamui.toml` file. On first run, Kamui starts an interactive onboarding
flow that asks for an OpenAI-compatible base URL and API key, discovers the available models, and
lets you choose the default model. It then saves the configuration and starts the chat immediately:

| Platform | Global config file |
| --- | --- |
| Windows | `%APPDATA%\\kamui\\kamui.toml` |
| Linux | `~/.config/kamui/kamui.toml` |
| macOS | `~/Library/Application Support/kamui/kamui.toml` |

The API key is entered through a hidden prompt. If the global config exists but is missing either a
usable model or API key, Kamui runs the same onboarding flow. Existing advanced multi-profile
configurations are never replaced automatically.

```toml
model = "gpt-5.5"
# context_window = 128000

[provider]
base_url = "https://api.openai.com/v1"
api_key = "sk-xxxxxxxx"
```

You may also place a `kamui.toml` in a project directory to override `model`, `context_window`, and
`provider.base_url` for that project. A project file **must not** contain an `api_key` — Kamui
rejects it, so a project config is safe to commit. The API key lives only in the global file.

Any service implementing the OpenAI Chat Completions API can be used by changing the base URL,
model, and API key. Chat responses use the API's SSE streaming mode and are rendered as deltas
arrive.

### OpenAI-compatible providers

Kamui talks to any OpenAI-compatible endpoint, so you can point it at hosted aggregators or a local
model without a dedicated integration. These are OpenAI-compatible services, not native providers;
tool use and streaming depend on the server and model you pick.

**OpenRouter** (many models behind one key):

```toml
model = "openai/gpt-4o-mini"   # any OpenRouter model id, namespaced as vendor/model
[provider]
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-v1-..."
```

**Ollama** (local models, no network key):

```toml
model = "llama3.2"                        # a model you have pulled with `ollama pull`
[provider]
base_url = "http://localhost:11434/v1"
api_key = "ollama"                        # Ollama ignores this, but a value is required
# tools = false                           # set this if the model rejects the tools field
```

Many small local models do not support tool calling and reject requests that include tools. If you
see an error like `<model> does not support tools`, add `tools = false` to that profile — Kamui then
chats without offering tools.

Only the global `kamui.toml` holds the key; switching providers is a one-file edit. You can also
keep a per-project `kamui.toml` that sets just `model` and `provider.base_url` to pin a project to a
particular provider (never the key).

### Switching models and providers at runtime

Instead of editing the file every time, define named profiles once and switch with `/model`. Share
one API key across many models by defining a `[providers.<name>]` block and pointing profiles at it
with `provider = "<name>"`:

```toml
default_profile = "sol"

[providers.jatevo]
base_url = "https://api.jatevo.ai/v1"
api_key = "sk-jvo-..."          # defined once, used by every Jatevo profile

[providers.ollama]
base_url = "http://localhost:11434/v1"
api_key = "ollama"

[profiles.sol]
provider = "jatevo"
model = "gpt-5.6-sol"

[profiles.terra]
provider = "jatevo"
model = "gpt-5.6-terra"

[profiles.codeqwen]
provider = "ollama"
model = "codeqwen:latest"
tools = false                   # this model does not support tools
```

In chat, `/model` lists the profiles and marks the active one, and `/model codeqwen` switches the
active provider and model for the next messages. Your choice is remembered across restarts, and the
banner always shows which model is active — handy for comparing the same prompt across models. A
profile can still set `base_url`/`api_key` inline instead of referencing a provider.

### Semantic search

Set an `embedding_model` on a profile (same provider, a different model than the one used for
chat) to enable `/index` and the `search_code` tool:

```toml
[provider]
embedding_model = "text-embedding-3-small"
```

Run `/index` to build the index, then ask a question the model can answer with `search_code`
instead of literal `grep`:

```text
> /index
Indexed 42 file(s) (118 new chunks), skipped 0 unchanged, removed 0 deleted. 118 chunk(s) total.

> Where do we validate the API key?
```

How it works: `/index` walks the project the same `.gitignore`-aware way `grep`/`glob` do, splits
each file into fixed 50-line chunks, and embeds any chunk from a file whose content changed since
the last run (tracked by a content hash, so re-running `/index` after a small edit only re-embeds
the files that actually changed). `search_code` embeds your query and ranks every stored chunk by
cosine similarity, returned as `path:start-end` with the chunk text — read the file for full
context. Without an `embedding_model` configured, `search_code` is not offered to the model at all
rather than erroring.

This is a v1 baseline: chunking is fixed-size line windows with no syntax awareness, and matching
is a brute-force scan (no vector index) — both simple choices that hold up fine at a single
project's scale. Project indexing at a larger scale, and richer chunking, are open future work.

### Skipping approval

Every `run_command`/`patch_file` call asks for approval by default, even for something as safe as
`git status`. Two ways to reduce that friction, in the global `kamui.toml` only — never in a
project file, since either one would let a checked-in file grant unattended execution:

```toml
[permissions]
allow_commands = ["git status", "git diff", "cargo check"]
```

`allow_commands` is an exact match (after trimming) against the whole command string, so
`"git status"` does not also cover `"git status --short"` — list each command you want to skip.

For everything else, start Kamui (or `-p`) with `--auto-approve` to skip every confirmation prompt
for the whole session:

```sh
kamui --auto-approve
kamui --auto-approve -r <session-id>
```

Kamui prints a banner when it starts this way, since it removes every safety prompt for the
session. `-p`'s `--auto-approve` (see [Non-interactive mode](#non-interactive-mode)) works the
same way for a single scripted turn.

## Install

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/algonacci/kamui/main/install.ps1 | iex
```

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/algonacci/kamui/main/install.sh | sh
```

Then open a new terminal and run:

```sh
kamui
```

Check the installed version with `kamui --version` and list command-line options with
`kamui --help`.

For development, install the current checkout into Cargo's binary directory:

```sh
cargo install --path .
```

This compiles once and installs `kamui` in `~/.cargo/bin`. It does not compile again each time the
command runs. Use `cargo install --path . --force` after local source changes.

## Development

```sh
cargo run
```

Kamui stores sessions, messages, and token usage in a local SQLite database. Each launch starts a
new chat, but it is only saved as a session after the first successful response. Use `/sessions`
and `/resume <id>` to continue an earlier conversation. Use `/help` inside the chat to list all
session commands.

The startup status card shows the Kamui version, project path, Git branch and changed-file count,
active model, available tools, connected MCP servers, and project instruction file. Run `/status`
at any time to refresh it after repository changes.

After the first response, the active provider generates a short session title. `/sessions` shows
that title together with the last-updated timestamp, message count, and total token usage. Tokens
used to generate the title are included in session usage analytics.

Resume a saved session directly when starting Kamui:

```sh
kamui -r <session-id>
```

### Non-interactive mode

Run a single prompt and exit, for scripting or CI:

```sh
kamui -p "explain what this project does"
```

The prompt runs through the same agent loop as interactive chat — including tools — and the
exchange is saved as a resumable session, just like a normal chat turn. Tool calls that need
approval (`run_command`, `patch_file`) are denied by default in this mode, since there is no one to
ask; pass `--auto-approve` to run them without prompting:

```sh
kamui -p "run the test suite and summarize failures" --auto-approve
```

### Diagnostics

Check that everything is configured and reachable without starting a chat session:

```sh
kamui doctor
```

It parses the config, sends a real test request to the default provider profile, connects to
every configured MCP server, and opens the database — printing a ✓ or ✗ line for each with an
actionable message on failure, and exiting with a non-zero status if anything failed. Useful as a
pre-flight check in a script, or just to check on things without leaving the terminal.

For a quicker look with no network calls at all:

```sh
kamui status
```

`status` reads the config file and local database directly and prints a summary — active profile
and model, configured profiles, `embedding_model`, MCP servers, the command allowlist, project
path, database path, session count, remembered-fact count, and indexed-chunk count. Unlike
`doctor`, it never contacts the provider or an MCP server, so it works even when those would fail.

### Session commands

| Command | Description |
| --- | --- |
| `/new` | Start a new session |
| `/sessions` | List saved sessions |
| `/resume <id>` | Resume a session |
| `/model [name]` | List provider profiles, or switch to one |
| `/rename <id> <title>` | Rename a session |
| `/search <text>` | Search saved messages across all sessions |
| `/compact` | Summarize older messages to free up context |
| `/undo` | Revert the files patched by the last turn |
| `/jobs` | List background jobs started with run_command |
| `/index` | Rebuild the semantic-search index (needs `embedding_model`) |
| `/delete <id>` | Delete a session |
| `/stats` | Show current session usage |
| `/memory` | List facts Kamui remembers across sessions and projects |
| `/forget <text>` | Forget one remembered fact, or `/forget all` |
| `/help` | List available commands |
| `/exit` | Save and quit |

`Ctrl+C` while a turn is running — waiting on the model, streaming, at an approval prompt, or
running a command — cancels that turn and returns you to the prompt, killing any running command.
The cancelled turn is not saved. At the idle prompt, `Ctrl+C` exits.

`/rename` accepts a session ID prefix followed by the new title; if the renamed session is the
active one, its in-memory title updates immediately. `/search` matches message text
case-insensitively (literal `%` and `_` are not treated as wildcards) and prints each hit as its
session ID, timestamp, title, and a snippet centered on the match.

After each streamed response, Kamui reports time-to-first-token (`TTFT`) and total response time
(`Time`) alongside token usage and the finish reason.

Long conversations are compacted automatically: once the recent history grows large, Kamui folds the
older messages into a running summary so the session can continue without overflowing the model's
context. Run `/compact` to do it on demand. The full history is always kept in storage — only what
is sent to the model each turn is compressed.

## Repository context

Kamui uses the directory where it was launched as the project root. If that directory contains
`KAMUI.md` or `AGENTS.md`, Kamui sends the first file found in that order as project instructions
with every chat request.

Reference a UTF-8 text file relative to the project root with `@path`:

```text
> Explain the error handling in @src/main.rs
```

Reference a directory to attach everything inside it:

```text
> Review the error handling across @src
```

Directory attachment honours `.gitignore` and skips hidden files, so build output and dependencies
stay out. Files are attached until the context budget or a 50-file cap runs out; the rest are noted
as omitted instead of failing the prompt.

Referenced files are attached only to that request and are not copied into session history. Each
file is limited to 64 KiB and all attached files together are limited to 128 KiB. Absolute paths,
binary files, and paths or symlinks outside the project are rejected.

Use `@diff` for unstaged tracked changes or `@staged` for changes in the Git index:

```text
> Review @diff for bugs
> Write a commit summary for @staged
```

Git context is read-only and can be combined with file references. Untracked files are not included
in `@diff`; attach them explicitly with `@path`.

Use `@clipboard` to attach whatever is on your system clipboard — text or an image:

```text
> Why does this happen? @clipboard
```

If the clipboard holds text (an error message, a stack trace, a snippet) it is attached as text. If
it holds an image, it is attached as an image — so you can take a screenshot (`Win+Shift+S` on
Windows, `Cmd+Shift+Ctrl+4` on macOS) and paste it straight into a prompt without saving a file
first. Terminals cannot receive pasted image data directly, so `@clipboard` is how images get in.

You can also reference an image file. `.png`, `.jpg`, `.jpeg`, `.gif`, and `.webp` are attached as
images rather than inlined as text:

```text
> What is wrong with this layout? @screenshot.png
```

Each image is limited to 5 MiB and is sent only with that request. The active model must support
image input; text-only models will reject it.

## Tools

Kamui offers the model these tools: `list_directory` (discover what is in a folder), `read_file`
(read a file), `grep` (search file contents by regular expression), `glob` (find files by a glob
pattern), `run_command` (run a shell command, optionally in the background), `command_status`/
`stop_command` (check on or stop a background job), `patch_file` (edit or create a file),
`update_plan` (declare a live checklist for a multi-step task), `spawn_agent` (delegate a
self-contained, read-only task to an isolated sub-agent), `search_code` (semantic code search,
only offered when `embedding_model` is configured — see below), `ask_user` (pause and ask you a
clarifying question), and `remember`/`update_memory`/`forget` (manage persistent memory — see
below). When you ask about code, the model can explore, read, build, test, and fix on its own
instead of requiring you to attach files with `@path`:

```text
> What does the agent loop in src/chat.rs do?
> Run the tests and tell me if anything fails.
> Fix the typo in the README heading.
```

`grep` and `glob` are read-only, so they never prompt for approval. Both respect `.gitignore` the
same way directory context (`@dir`) does. `grep` takes a regular expression plus optional `path`
(scope), `glob` (filename filter), and `case_insensitive`; `glob` takes a pattern like
`"src/**/*.rs"` plus an optional `path` scope. The model is steered to prefer these over shelling
out to `grep`/`find` through `run_command`, since they need no approval and are cheaper to run.

For multi-step tasks, the model can call `update_plan` with a checklist (`{step, status}` for each
step, replacing the whole list each call) instead of leaving its progress buried in the raw tool
trace. Kamui renders it live as a checklist:

```
  → plan
    [x] Explore the codebase
    [~] Implement the change
    [ ] Run tests
```

`[x]` is completed, `[~]` is in progress, `[ ]` is pending. Like `grep`/`glob`, it never prompts
for approval — it has no effect outside the conversation and round-trips through session history
like any other tool call, so a resumed session shows the plan as it stood at each point.

If the model calls a tool, Kamui prints a short trace of each call, runs it, feeds the result back,
and continues streaming until a final answer. The read tools reuse the same path safety as `@file`
(project-relative only, no escaping the root, 64 KiB per file) and the loop is bounded so it cannot
run away.

`run_command` never runs on its own. Kamui shows you the exact command and waits for you to approve
it (`y`/`yes`); anything else declines and tells the model so. A third answer, `a`/`always`, both
approves this call and grants that *tool* (not that specific command — every future call to it,
e.g. any `run_command` invocation) a standing pass for the rest of the active session, so further
calls skip the prompt entirely until `/new` clears it. This is separate from the global
`[permissions] allow_commands` allowlist below: "always allow" is a one-tap, session-only grant you
make from the prompt itself, not something you configure ahead of time. Commands run in the project
directory with input disabled and a capped amount of captured output, and the model sees the exit
code alongside stdout and stderr. The foreground timeout defaults to 30 seconds and is configurable —
see `[commands]` below.

For something that legitimately runs longer than that — a dev server, a slow test suite — the model
can pass `background: true` instead. It returns a job id immediately rather than waiting, and can
check on it with `command_status` (omit `job_id` to list every job) or stop it early with
`stop_command`. `/jobs` lists them directly without going through the model. Background jobs are
in-memory only: they are killed when Kamui exits (including a plain `-p` run) and do not survive a
restart, and each has a `background_max_secs` (default 30 minutes) safety cap against a runaway or
zombie process — not a limit meant to constrain a legitimately long-running command.

```toml
[commands]
timeout_secs = 30            # foreground run_command
background_max_secs = 1800   # safety cap for a background: true job
```

`patch_file` edits one file per call and is also gated behind your approval (the same `y`/`yes`/
`a`/`always` prompt): Kamui shows the change as removed (`-`) and added (`+`) lines before asking. A
patch replaces text that must match the file
exactly once — if it does not, the patch is rejected and the model is told to re-read the file, so a
stale edit can never overwrite unexpected content. An empty `old_text` creates a new file. Writes are
atomic per file, and paths cannot escape the project root.

Each file `patch_file` touches is approved individually, exactly as before, but Kamui also keeps a
snapshot of what every touched file looked like before the turn started. If a multi-file edit is
interrupted with `Ctrl+C` partway through, the files it already changed are automatically reverted
so the turn never leaves the repository half-edited with no trace in session history. `/undo`
reverts the same way for a turn that *did* complete — one level, most recent turn only; a second
`/undo` has nothing left to do.

If the [RTK](https://github.com/rtk-ai/rtk) binary is installed, simple approved commands are
automatically prefixed with `rtk` so their output is compressed before it reaches the model. RTK is
optional: commands with shell operators and systems without RTK always run directly, and the first
line of every result shows the exact command that ran.

The whole turn is saved to session history, including the tool calls and their results, so a resumed
session replays the tool interactions the model relied on.

`ask_user` pauses a turn so the model can ask you something instead of guessing — which of several
matching files was meant, a preference between reasonable options. Kamui prints the question (with
numbered choices, if any) and waits for one line of input; replying with a number resolves to that
option's text, and anything else is taken as typed. This is not an approval prompt — `run_command`
and `patch_file` still ask for that automatically — it's for when the model itself decides it needs
more information to continue. In `-p` non-interactive mode there is no one to ask, so the tool is
declined and the model is told to proceed on its own judgment instead.

## Sub-agents

`spawn_agent` delegates a self-contained task to an isolated sub-agent and returns only its final
answer — the sub-agent's own tool calls and intermediate output never enter the main
conversation's context:

```text
> Summarize how error handling works across the codebase
```

The model might call `spawn_agent` internally to explore before answering, instead of doing that
exploration in the main conversation itself. The sub-agent:

- Starts fresh with no memory of the conversation, so its prompt must be self-contained.
- Can only `read_file`, `list_directory`, `grep`, and `glob` — it cannot run commands, edit files,
  touch memory, or spawn another sub-agent, so it never needs your approval for anything it does.
- Runs sequentially (it blocks the turn until it returns), not concurrently with the main
  conversation — a deliberate v1 scope, since concurrent sub-agents would need their own approval
  prompts interleaved with the main one.

This is a good fit for a well-scoped question like "find every place X is used and summarize how"
or "explain what module Y does," not a substitute for tools the model can already call directly.

## Memory

Kamui can remember facts across every session and project, not just the current conversation. Ask
it directly, or state a clear preference:

```
> Remember that I prefer bun over node, and uv over pip.
```

The model calls `remember` to store the fact permanently. Unlike project instructions (`KAMUI.md`,
a file you edit yourself) or session history (scoped to one conversation), memory is global: it
survives `/new`, resuming a different session, and switching to a different project entirely, and
is read fresh on every turn — no restart needed to see a fact you just asked it to remember.

- `/memory` lists everything currently remembered.
- `/forget <text>` removes one fact matched by an unambiguous substring of its exact wording, or
  `/forget all` clears everything.

The model can also correct or remove a fact itself: `update_memory` replaces an existing entry
matched the same way `/forget` does, so a stated preference can be corrected in place instead of
contradicting an older one; `forget` removes one. Both fail with a clear error if the text matches
more than one stored fact, asking for something more specific rather than guessing. Total stored
memory is capped at 4 KiB, since every byte of it is sent with every request.

## MCP servers

Kamui is an [MCP](https://modelcontextprotocol.io) client. Declare servers in your **global**
`kamui.toml` and Kamui launches them at startup, offering their tools to the model alongside the
built-in ones:

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

[mcp.excel]
command = "uvx"
args = ["mcp-excel"]
trusted = true          # skip the per-call approval for this server
```

Their tools appear as `<server>__<tool>` (for example `excel__read_sheet`) so they cannot collide
with the built-ins. Every MCP call asks for your approval first — third-party servers can do
anything — unless you mark that server `trusted`. A server that fails to start is reported and
skipped, so a broken entry never prevents Kamui from running.

Servers may only be declared in the global config: launching one is arbitrary code execution, so a
checked-in project file is not allowed to do it. Only stdio servers are supported today.

## RTK integration direction

[RTK](https://github.com/rtk-ai/rtk) is an optional execution backend for the `run_command` tool.
Supported commands run through RTK so compact test, build, search, Git, and container output reaches
the model. Kamui still owns command permissions, timeouts, cancellation, output limits, and the
recorded command line; RTK only compresses output.

RTK is not currently used by chat or `@diff`, and users do not need to install it yet. Keeping raw
diff context avoids dropping details needed for code review. The future integration will detect the
external `rtk` binary and fall back to direct execution when it is unavailable or a command is not
supported.

## Data storage

The database uses the standard local application data directory for Windows, macOS, and Linux.
Set `KAMUI_DATA_DIR` to override it, particularly for servers and containers:

```env
KAMUI_DATA_DIR=/var/lib/kamui
```

SQLite is bundled into the Kamui binary, so users do not need to install it separately. For a
container deployment, mount `KAMUI_DATA_DIR` as a persistent volume. Each device has its own local
database; future multi-device synchronization should exchange records through an API rather than
copying the database file.
