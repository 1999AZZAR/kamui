# Completed Tasks

## 2026-09-04 — PR #18 staging→main (TUI UX package)
- **What:** Committed session work to `staging` (force-push over diverged remote per user pick) + opened PR #18 with full changelog.
- **Key files:** `src/chat.rs`, `src/ui.rs`, `src/markdown.rs`, docs
- **Verify:** `cargo test` 398 passed pre-commit; PR https://github.com/algonacci/kamui/pull/18

## 2026-09-04 — Markdown table support (both renderers)
- **What:** `split_table_row` + table branch in `render_line` (ANSI) and `render_ratatui`: `| a | b |` → cells joined by dim `│` with inline style per cell; separator row (`|---|---|`) → dim `─` rule. No column alignment (per-line wrapping can't guarantee it) — pipes gone, content keeps code/bold.
- **Key files:** `src/markdown.rs` split_table_row + 2 branches + 2 tests
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 398 passed

## 2026-09-04 — Sidebar semantic colors + markdown marker stripping
- **What:** Sidebar: `sidebar_value_style` now applies to every group (was gated to Last turn/Activity, so Runtime stayed flat) — model blue, build green / plan amber, dirty git amber, cache cyan. Markdown: both renderers strip block markers — `## X` → bold `X`, `- x` → bold `• x` (ordered keeps `1.`), `> q` → `│ q`, `---` → `─` rule, fences collapse to dim `··· lang`. Inline `**`/`code` unchanged (glob/snake_case guards intact).
- **Key files:** `src/ui.rs` push_sidebar_value, `src/markdown.rs` render_line + render_ratatui
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 396 passed (4 expectations updated + new ratatui strip test)

## 2026-09-04 — Sidebar variant A + C indicators (prototype winner folded in)
- **What:** `update_sidebar` groups entries into Session/Runtime/Context/Activity/Last turn (tab-separated metric rows); Context gains `bar\tpct` usage bar; turn call site adds `Activity` (tools count + lat + time). Renderer: section headers as muted rules, semantic value ink (model blue, plan amber/build green, dirty git amber, cache cyan, bar green→amber→red), `▓░` bar from `bar\tNN`.
- **Key files:** `src/chat.rs` update_sidebar + entries_push + 6 call sites, `src/ui.rs` sidebar_paragraph/is_sidebar_section/sidebar_value_style/push_sidebar_value + CYAN, `docs/ui-architecture.md`
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 395 passed (incl. new sidebar groups test)

## 2026-09-04 — Cache-miss detection + frozen summary template (P1.4/P1.5)
- **What:** P1.4: `cache_miss_label` (noise floor 1024, Pi parity) — diam di first turn/hit, label sebab bila miss (`miss` / `miss (model switch)` / `miss (prefix rebuilt)`); tampil di usage line + sidebar cache row; state `prev_cached`/`prev_model`/`head_rebuilt_this_turn`. P1.5: instruction summary jadi konstanta frozen `SUMMARY_INSTRUCTION` + `NO_PRIOR_SUMMARY` (do-not-edit note); usage compaction memang sudah segregasi (`kind` filter) → tanpa perubahan storage.
- **Key files:** `src/chat.rs`, `src/compaction.rs`; commit `7bc063b`
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 406 passed (3 test baru: silent-first/hit, cause labels, summary shape stable)

## 2026-09-04 — Fixed tool array + frozen head per Session (P0.2/P0.3)
- **What:** P0.2: `tool_definitions` dihitung sekali per Session (`session_tools`), Plan Mode pending tidak susutkan roster — mutating call di-hold saat eksekusi (`is_mutating_held` sudah ada); hapus `ToolRegistry::plan_mode` + `plan_mode_definitions` yang mati. P0.3: `build_head_messages` pecah message[0] jadi pesan per blok stabil (base/memory/skills); summary Compaction pesan terpisah; memory re-read DB hanya pasca memory-tool (`memory_dirty` + `cached_memory_snapshot`); `run_once` selaras.
- **Key files:** `src/chat.rs`, `src/tools.rs` (-24); commit `38ae60e`
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 403 passed (2 test baru: skip empty blocks, blocks separate + memory-only diff)
- **Review note:** caught stale-memory bug (rebuild pasca skill-toggle baca `String::new`) → fixed via cached snapshot

## 2026-09-04 — Stable prompt_cache_key per Session (P0.1)
- **What:** `OpenAIRequest`/`OpenAIStreamRequest` kirim top-level `prompt_cache_key` = session id clamp 64 char unicode-safe (`clamp_prompt_cache_key`, Pi parity). `skip_serializing_if` omit saat tanpa session → backend generik zero wire change.
- **Key files:** `src/provider/openai.rs` (+71, 1 file); commit `d48752c`
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 401 passed (3 test baru: clamp 64/unicode-safe, omit tanpa session, follows session_id)
- **Caveat:** `prompt_cache_key` param Responses API — efek di `/chat/completions` tergantung backend honor atau tidak; stabilitas body (P0.2/P0.3) tetap langkah berikutnya

## 2026-09-04 — Cache-hit gap analysis doc + riset clone
- **What:** Clone `pi-cache-hit-research.md` ke `docs/research/` (copy, bukan pindah) + tulis `docs/research/kamui-cache-hit-gap-analysis.md`: mengapa Pi unggul (5 layer terverifikasi + koreksi), kamui lacks per layer, ranking 3 prefix-breaker, roadmap P0/P1/P2.
- **Key files:** `docs/research/pi-cache-hit-research.md` (clone), `docs/research/kamui-cache-hit-gap-analysis.md` (new)
- **Verify:** semua klaim kamui file:line-grounded (`src/chat.rs:982-1070`, `src/provider/openai.rs:133-148`, `src/compaction.rs:71-88`); grep zero-hit `cache_control|prompt_cache_key|retention` confirmed

## 2026-09-04 — Cache-hit comparison pi-mono vs kamui (3-scout audit)
- **What:** Verified pi-cache-hit-research.md against live pi-mono source + audited kamui request path vs Pi 5 layers.
- **Pi corrections:** "exactly 3 breakpoints" wrong — OAuth emits 4 blocks (`anthropic-messages.ts:1070,1077`), count shrinks with gates (retention none / no-tools / `supportsCacheControlOnTools`); no `5min` literal — TTL-omitted = server default (`getCacheControl:73`); no `sort()` — order stability = caller-order + normalized-name dedupe (`deferred-tools.ts:14-30`); replay byte-identical only same-model + signature present (sanitize `:1317`, empty-drop `:1240,1264-1330`, missing-signature downgrade `:1301-1312`, cross-model rewrite `transform-messages.ts:104-142`); `buildParams/convertMessages/convertTools/getCacheControl` all module-private; types at `src/types.ts` not `api/types.ts`. OpenAI: completions `store:false` conditional (`openai-completions.ts:822-824`), extra `x-session-affinity` header (`:766-774`), `openai-nosession` variant omits session_id.
- **Kamui verdicts:** L1 prefix stability PARTIAL — serialization deterministic but message[0] rebuilt every turn (fresh `list_memory` + growing summary + skill block, `chat.rs:1047-1061`) and window slides (`chat.rs:1062-1063`); L2 breakpoints ABSENT (OpenAI-only wire, grep `cache_control|prompt_cache_key|retention` = zero hits in src/); L3 affinity PARTIAL — sticky top-level `session_id` Orvix-gated (`openai.rs:66-81`, default off `config.rs:42-45`), no `prompt_cache_key`/headers; L4 replay PRESENT for tool ids/args verbatim (`storage.rs:700-708` → wire, test `openai.rs:~770`), thinking N/A; L5 hygiene ABSENT — no retention concept, `/model` switch keeps history (guaranteed miss), compaction/title ad-hoc shapes with dropped usage. Observability read-only: `cached_tokens` parsed/stored/shown, never acted on.
- **Cheapest fixes:** (1) freeze prefix — split message[0] into fixed messages, cache `list_memory()` per session; (2) one fixed tool array per session — hoist out of plan-mode branch (`chat.rs:982-990`), always serialize same array; (3) add `prompt_cache_key` = stable `coding_session_id` (~5 lines, `openai.rs:135-161`).
- **Key files:** kamui `src/provider/{mod,openai}.rs`, `src/chat.rs`, `src/compaction.rs`, `src/tools.rs`, `src/storage.rs`, `src/prompt.rs`; pi-mono `packages/ai/src/api/{anthropic-messages,openai-responses,openai-completions,openai-prompt-cache,transform-messages}.ts`, `packages/coding-agent/src/core/cache-stats.ts`
- **Verify:** 3 parallel scouts, every claim file:line-grounded; kamui zero-hit grep confirmed

## 2026-09-04 — TUI UX 3-variant prototype (mockup, awaiting pick)
- **What:** Single-file HTML mockup `/tmp/kamui-tui-ux-prototype.html` with `?variant=A|B|C` + floating switcher (arrows/keys). A rail hierarchy, B minimal chat, C execution timeline. All share rendered markdown, tool states, truncation, context bar + cache.
- **Inspected:** `src/ui.rs` card_lines/sidebar_paragraph/transcript, `src/markdown.rs` render_ratatui (narrow subset), `src/terminal.rs` styles, `src/render.rs` plain, `docs/ui-architecture.md`
- **Verify:** headless browser check — each variant renders exactly one visible panel with correct label; screenshots captured A/B/C
- **Next:** user picks winner (or mix); fold into `src/ui.rs`, rest to throwaway branch per prototype skill

## 2026-09-04 — Enter accepts slash completion
- **What:** Handler Enter di `input_thread`: bila slash menu terbuka + ada kandidat + buffer bukan exact match → resolve pilihan `selected` jadi `/nama ` lalu submit langsung. Backslash-escape dicek dulu. Tab tetap accept-tanpa-submit. Hint help `?` diperbarui.
- **Key files:** `src/ui.rs` Enter handler + help rows
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 394 passed

## 2026-09-04 — Sidebar Last turn cache row
- **What:** Payload `Last turn` tambah baris `cache\tN (P%)` di antara `in` dan `out` (guard `> 0`, clamp 100%, format tab existing).
- **Key files:** `src/chat.rs` turn payload builder
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 394 passed

## 2026-09-04 — Sidebar Context cache hit rate
- **What:** `update_sidebar` terima `last_cached_tokens`; entry Context tambah baris `Cached: N (P%)` (format `/stats`, guard `> 0`, clamp 100%). 2 call site turn isi dari `usage.cached_tokens`, 4 call site lain `None`.
- **Key files:** `src/chat.rs` update_sidebar + 6 call sites
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 394 passed

## 2026-09-04 — Codebase exploration (5-slice scout map)
- **What:** Mapped entry/config, provider, agent loop/tools, UI/TUI, storage/jobs via 5 parallel scouts.
- **Key files:** `src/main.rs`, `src/config.rs`, `src/provider/openai.rs`, `src/chat.rs`, `src/tools.rs`, `src/ui.rs`, `src/render.rs`, `src/storage.rs`, `src/jobs.rs`
- **Findings:** OpenAI-compat single provider; agent loop ≤25 rounds with plan/approval gates; god-files chat.rs (~6.1k) + tools.rs (~2.7k); TUI audit Fase A done, sisa B-presence + D-filter; SQLite v11 + resume; backlog = Dispatch + reliability sprint.

## 2026-08-23 — Pi-like full-bleed timeline (PR #14 `improvement`)
- **What:** `src/render.rs` box_lines (unicode ┌─┐└┘│, width-aware) + 8 renderers: User (BgBlue+White), Tool Call (BgGray `Tool: <name>`), Tool Output (boxed truncated 20 lines/1000 chars), Progress `⠋` DIM, System `───` dim, Warning `─── ⚠` yellow, Error `┌─ Error ─┐` BgRed, Assistant/Final plain. Wired into `chat.rs` all 8 sites: non-TUI user prompt boxed, tool calls both loops (plan+generic, interactive + -p), tool outputs preview, sub-agent progress, New chat/Resuming system, --auto-approve warning, 3× Request failed error boxes. Hoisted Ui::stdio().
- **Why:** Spec wants pi.dev execution-log hierarchy (not chat bubbles) with distinct bg/border per event type, full-width boxes distinct from prior chip colors.
- **Key files:** `src/render.rs` new, `src/chat.rs` wiring + preview_output helper/test, `src/main.rs` mod render, `src/terminal.rs` Bg* Styles already
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 280 passed, manual `cargo run` boxed timeline
- **Prototype:** `/tmp/kamui-pi-prototype.html` + `/tmp/kamui-pi-prototype.html` (pi timeline)
- **Commits:** `d3e42df` feat(render)

## 2026-08-23 — /skills dropdown + per-kind CLI colors (PR #14 `improvement`)
- **What:** Fix /skills inline wrap → windowed dropdown (10 rows, term-width truncation 20..60, header "Skills · ↑/↓ navigate · Enter toggle · Esc close", grouped by source, NO_COLOR fallback) + CLI palette (user prompt white-on-blue, thinking DIM, tool call gray BgGray, outcome green BgGreen/red BgRed, errors red)
- **Why:** Screenshot showed 300+ char skill descs wrapping and ghosting via last_lines miscount; CLI plain no per-kind distinction
- **Key files:** `src/chat.rs` run_skills_popup + print_skills, `src/terminal.rs` Style enum + Ui::style, `src/markdown.rs` consts consolidated, `src/tui.rs` prompt echo (BgBlue+White full line)
- **Verify:** `cargo fmt` pass, `clippy -D warnings` pass, `cargo test` 275→280 passed, `cargo run` /skills manual 10-row scroll, colored feed
- **Prototype:** `/tmp/kamui-prototype.html` (dropdown + feed), `/tmp/kamui-pi-prototype.html` (pi)
- **Commits:** `2c82b4e` slash truncation, `b32696d`/`e87645c` ui bg, `d3e42df` render
