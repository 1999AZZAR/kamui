# Anthropic-compatible API — native adapter vs shim

> Wayfinder #4 · 2026-08-22 · ponytail: shortest doc that answers (a)–(e), link sources inline.

## TL;DR

**Shim cukup untuk Sprint C; native adapter ditunda.** OpenAI-compat proxy (LiteLLM/openai-to-anthropic) bisa bawa Kamui ke model Claude hari ini tanpa file baru, tapi **prompt caching & extended thinking tidak lewat utuh** — ada bug terbuka LiteLLM #18950 (thinking + prompt_caching gagal) dan shim men-drop `cache_control`/`thinking` fidelity. Cache-hit visibility Anthropic (`cache_read_input_tokens` / `cache_creation_input_tokens`) **tidak butuh native adapter** — cukup dual-path `Usage` deserializer (sudah diputus di #2, Sprint A). Native `src/provider/anthropic.rs` ≈ **8 SP** (sudah di SPRINT_PLAN C2), layak dikerjakan **setelah Sprint A+B stabil + evaluasi**, atau saat ada user demand / benchmark tunjukkan shim jadi bottleneck. Jangan kejar parity penuh Codex/Claude Code — jaga DNA Kamui: small, auditable, no silent spend.

## (a) Wire format — Messages vs Chat Completions

| Dimensi | OpenAI `POST /v1/chat/completions` | Anthropic `POST /v1/messages` |
|---|---|---|
| System prompt | `messages[].role="system"` | Top-level `system: string \| ContentBlock[]` (bukan di `messages`) |
| Message content | `string` atau `parts[]` (text+image_url) | `ContentBlock[]` wajib array (`text`/`image`/`tool_use`/`tool_result`/`thinking`) |
| max_tokens | opsional | **required** |
| Tools schema | `tools[].function.parameters: JSON Schema` (`type:"function"`) | `tools[].input_schema: JSON Schema` + `name`/`description` langsung |
| Tool call (assistant) | `tool_calls[]` (`id`, `type:"function"`, `function:{name,arguments}`) | `content: [{type:"tool_use", id, name, input: object}]` |
| Tool result (user) | `role:"tool"` + `tool_call_id` + `content: string` | `role:"user"` + `content: [{type:"tool_result", tool_use_id, content, is_error?}]` — **harus di awal message setelah `tool_use`** |
| Stop reason | `finish_reason: "stop"\|"tool_calls"\|"length"` | `stop_reason: "end_turn"\|"tool_use"\|"max_tokens"` |
| Streaming event | `data: {choices:[{delta:{content,tool_calls[]}, finish_reason}], usage?}` + `data: [DONE]` | Typed SSE: `event: message_start` → `content_block_start` → `content_block_delta` (text_delta/input_json_delta) → `content_block_stop` → `message_delta` (stop_reason+usage) → `message_stop` (+ `ping`/`error`) |
| Local mapping | `src/provider/mod.rs:26 Message{role,content,images,tool_calls,tool_call_id}` + `src/provider/openai.rs:95 WireMessage` | Perlu `system` terpisah + `tool_use`/`tool_result` di content array + `max_tokens` |

Lossy translation: `tool_result.is_error` (Anthropic) tidak ada di OpenAI; `thinking` blocks tidak ada padanannya; `system` array vs string beda caching marker. Mapping mekanis bisa, tapi fidelity turun untuk fitur premium.

Sources: Anthropic Messages API reference (docs.anthropic.com / docs.claude.com), AsyncAPI streaming spec, anthropic_gleam streaming types.

## (b) Shim vs native — apakah proxy cukup?

**Shim (LiteLLM proxy, y-router, claude-code-proxy, openai-to-anthropic adapters):**

- Pro: zero-code di Kamui — cukup ganti `base_url` ke proxy yang translate `openai/* → anthropic/*`. Validasi cepat, tidak tambah maintenance surface.
- Kontra untuk fitur premium:
  - **Prompt caching**: Anthropic caching pakai `cache_control: {type:"ephemeral"}` per block (system/text/tool_use/tool_result/tools) + TTL 5 menit (refresh on hit). LiteLLM proxy support caching tapi **routing & token counter belum handle `thinking` content type** — issue #18950: Router `prompt_caching` + Extended Thinking gagal karena counter tidak kenal `thinking` block. `cache_control` juga butuh `system` dalam bentuk array; string plain tidak bisa di-cache.
  - **Extended thinking**: `thinking` content blocks hanya native. Shim yang map ke OpenAI `reasoning_effort`/`thinking` sering tidak round-trip utuh termasuk streaming (input_json_delta vs thinking delta beda event).
  - Fidelity tool streaming: OpenAI `tool_calls[].index` incremental vs Anthropic `content_block_delta: input_json_delta` — proxy harus reassembly; bug edge mudah muncul.

**Native adapter (`src/provider/anthropic.rs`):**

- Pro: akses penuh `cache_control`, `thinking`, `cache_creation_input_tokens`/`cache_read_input_tokens`, event-typed SSE tanpa loss.
- Kontra: file baru + SSE parser baru + maintenance permanen; SPRINT_PLAN C2 sudah tandai 8 SP dan ROADMAP descoped — butuh keputusan produk.

**Keputusan:** Shim cukup untuk **validasi & unblock user Claude** sekarang. Native hanya wajib saat Kamui butuh **caching/thinking sebagai fitur first-class** (mis. prompt besar berulang, reasoning trace). Sampai itu ada demand/benchmark, shim adalah jawaban malas yang benar.

Sources: LiteLLM docs (prompt_caching, Anthropic provider), LiteLLM issue #18950, FastRouter Anthropic Messages format, Medium streaming explainer.

## (c) Capability flags di `Provider` trait

Trait hari ini (`src/provider/mod.rs:173`):

```rust
#[async_trait] pub trait Provider: Send + Sync {
  fn name(&self) -> &'static str;
  async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
  async fn chat_stream(&self, request: ChatRequest) -> Result<mpsc::UnboundedReceiver<Result<StreamEvent>>>;
  async fn embed(&self, model: &str, input: Vec<String>) -> Result<Vec<Vec<f32>>>;
}
```

Tidak ada capability flags — `chat`/`chat_stream`/`embed` implisit. Untuk tetap netral tanpa sebar `if provider == "anthropic"` di `chat.rs`:

- **Minimal sekarang (ponytail):** tidak tambah flag. `Usage` sudah netral; shim tidak butuh flag.
- **Saat native adapter mendarat:** tambah satu enum/flags kecil, mis. `ProviderCaps { supports_prompt_caching: bool, supports_thinking: bool, supports_vision: bool }` atau `fn caps(&self) -> Caps` — dipakai `chat.rs` untuk memutuskan apakah kirim `cache_control`/`thinking` dan bagaimana render `Usage`. Jangan tambah flag per-provider stringly-typed; satu struct `Caps` cukup. Ini align dengan saran KAMUI_ANALYSIS: capability-based registry sebelum native provider baru.

## (d) Prasyarat cache-hit Anthropic

**Tidak.** Ticket #2 sudah putuskan: `Usage` tambah `cached_tokens: u64` + deserializer dual-path yang baca `prompt_tokens_details.cached_tokens` (OpenAI) **dan** `cache_read_input_tokens` (Anthropic) tanpa tunggu native adapter. `print_usage`/`stats`/`usage` + migrasi `usage_records` v11 ikut. Jadi visibility cache-hit Anthropic bisa lewat shim sekalipun — yang hilang tanpa native hanya **kemampuan mengirim `cache_control` breakpoints** untuk *menciptakan* cache hit, bukan *membacanya*.

## (e) Effort vs Sprint C

- SPRINT_PLAN C2: **Native Anthropic provider — 8 SP, P2, Impact Sedang-Tinggi**, deskripsi: "Adapter kedua pakai tipe netral yang sudah ada (`ToolDefinition`/`ToolCall`/`StreamEvent`). Buka model Claude tanpa proxy." Cons: maintenance permanen, SSE beda, ROADMAP descoped.
- Urutan eksekusi rekomendasi: `A1(retry) → A5(timeout) → A2(parallel) → A4(bounded I/O) → A3(token-aware) → B1(plan mode) → B2(checkpoint) → baru C2/C3/C4/C1` — jadi C2 **di luar next sprint (2 minggu)**.
- Estimasi file: `src/provider/anthropic.rs` baru (~400–600 LOC: request/response types + `wire_messages` versi Anthropic + `read_stream` event-typed + tests mirip `openai.rs:582`), `src/provider/mod.rs` tambah `Caps` (opsional), `src/main.rs`/`config.rs` wiring profile → provider selection. Tidak sentuh `chat.rs` agent loop bila `Caps` dirancang benar.
- Rekomendasi: **jangan masukkan C2 ke Sprint A+B.** Kerjakan setelah A+B hijau + benchmark tunjukkan shim bottleneck atau ada user request eksplisit. Shim + `Usage` dual-path sudah unblock Sprint C tanpa native.

## Keputusan yang diminta (untuk grilling/planning berikutnya)

1. **Terima shim sebagai jawaban Sprint C** — dokumentasikan `base_url` proxy di README, tidak tambah kode.
2. **Lock `Usage.cached_tokens` dual-path di Sprint A** (sudah diputus #2) — ini satu-satunya prasyarat cache-hit.
3. **Tunda native adapter** sampai trigger: (i) user demand Claude native, atau (ii) benchmark perlu `cache_control`/`thinking` first-class. Saat trigger tercapai, eksekusi C2 (8 SP) dengan `Caps` minimal.

## Out of scope (tetap di-defer)

GUI, voice, plugin marketplace penuh, native Gemini — sesuai ROADMAP & map Out of scope. Tidak dibahas di tiket ini.
