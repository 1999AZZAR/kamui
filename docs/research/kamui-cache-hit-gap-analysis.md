# Cache-hit gap analysis: mengapa Pi unggul, kamui kurang di mana, roadmap perbaikan

> 2026-09-04 · sumber: `docs/research/pi-cache-hit-research.md` (clone dari
> `/Users/amalanfadil/dev/learn-harness/pi-cache-hit-research.md`, bukan pindahan) +
> verifikasi 3-scout paralel terhadap pi-mono live (`/tmp/pi-mono`, shallow clone)
> dan codebase kamui. Semua klaim di-grounding file:line.

## TL;DR

Pi cache-hit tinggi bukan karena satu trik, tapi karena **request construction
diperlakukan sebagai cache contract**: prefix byte-identik antar turn (L1),
breakpoint eksplisit (L2, Anthropic), routing affinity (L3, OpenAI), replay
byte-identik (L4), hygiene request sampingan (L5). Kamui hari ini hanya memenuhi
L4 (tool-call IDs) dan sebagian L1/L3 — tiga prefix-breaker terbesarnya adalah
**message[0] di-rebuild tiap turn**, **roster tools flip-flop**, dan **tanpa
affinity key di backend generik**. Tiga fix termurah menutup sebagian besar gap
tanpa file baru.

## 1. Cara kerja prompt caching (mengapa prefix = kontrak)

Prompt cache hit **hanya bila prefix request byte-identik** dengan request
sebelumnya. Provider tidak mem-cache "makna", tapi byte prefix. Konsekuensinya:

- Perubahan 1 byte di awal (system prompt) → seluruh suffix miss, semua token
  di-bill ulang.
- Perubahan di ekor (pesan user baru) → hanya ekor yang di-bill; prefix hit.
- Maka desain request harus: **kepala frozen, ekor append-only**.

Ini fondasi yang menjelaskan semua 5 layer Pi di bawah.

## 2. Mengapa Pi unggul (5 layer, terverifikasi + koreksi)

### L1 — Prefix stability (fondasi, gratis semua provider)

Sanitize surrogates di semua string (`anthropic-messages.ts:138,146,1076,1085,
1243-1317`), filter deterministik (drop empty/whitespace `:1240,1264-1330`),
tool order = caller-order + normalized-name dedupe (`deferred-tools.ts:14-30`).

Koreksi dari riset: **tidak ada `sort()` di mana pun** — "stable" artinya
caller-order preservation, bukan sorted. Filter juga lossy tapi deterministik:
input sama → byte sama; input kosong-berbeda → prefix shift.

### L2 — Breakpoint eksplisit (Anthropic)

`cache_control: {type:"ephemeral"}` di 3 lokasi: system (`:1066-1089`), tool
terakhir (`:1458`), blok terakhir user msg (`:1373-1393`). TTL `1h` iff long
(`getCacheControl:73`); else omit = server-default 5 menit.

Koreksi dari riset: **"exactly 3" salah** — OAuth emit **4 blok** (`:1070`
identity + `:1077` prompt, menyentuh max-4 Anthropic); count menyusut bila
retention none / no-tools / `supportsCacheControlOnTools=false`. Tidak ada
literal `5min` di source. Semua fungsi (`buildParams`, `convertMessages`,
`convertTools`, `getCacheControl`) module-private, dan types di `src/types.ts`
bukan `api/types.ts`.

### L3 — Routing affinity (OpenAI)

`prompt_cache_key` = sessionId clamp 64 char unicode-safe
(`openai-prompt-cache.ts:1-8`); `prompt_cache_retention:24h` hanya bila long
(`responses.ts:83-86`); header `session_id` + `x-client-request-id` vs
`x-session-id` (openrouter); `store:false`. Rasional: same-prefix-different-replica
= miss, hence headers.

Nuansa: completions `store:false` conditional (`openai-completions.ts:822-824`),
ada header ketiga `x-session-affinity` (`:766-774`), varian `openai-nosession`
omit session_id.

### L4 — Exact replay opaque blobs

Same-model verbatim (signature `:1318`, tool IDs `:1324`) **dengan carve-out**:
thinking text tetap di-sanitize (`:1317`), empty-drop, missing-signature
downgrade ke text (`:1301-1312`). Cross-model rewrite total
(`transform-messages.ts:104-142`); `normalizeToolCallId` hanya cross-model
(`:1178-1179`). Jadi "byte-identical" hanya same-model + signature present.

### L5 — Hygiene + observability

Compaction/structural paksa `cacheRetention:none` + fresh sessionId
(`compaction.ts:121-123`, `structural.ts:786-787`) — request sampingan tidak
menulis cache sia-sia / menggeser breakpoint. Model switch **dihitung miss,
bukan di-exempt** (`cache-stats.ts:113-116`); compaction reset baseline
(`:112-113`); idle>TTL dilabel `idleMs` (`CACHE_TTL_MS=5min :8`) agar
timeout-miss tidak disalahkan ke harness. Observability: footer `R/W/CH`,
`NOISE_FLOOR=1024` (`:11`), notice gate 20k token + $0.10
(`interactive-mode.ts:3857`), setting `cache-miss-notices`.

## 3. Kamui lacks di mana (per layer, file:line)

Arsitektur kamui: OpenAI `chat/completions`-only (`Provider::name()` =
`"openai"`, `openai.rs:460`; body `OpenAIRequest`/`OpenAIStreamRequest`,
`openai.rs:133-148`). Tanpa Anthropic shape, tanpa `/responses`, tanpa
`reasoning`/`thinking`/`temperature`/`tool_choice` di wire.

| Layer | Status | Bukti |
|---|---|---|
| L1 stabilitas | PARTIAL | Serialisasi deterministik (`Vec` order, `wire_message` verbatim `openai.rs:~225-268`), **tapi message[0] di-concat ulang tiap turn**: `prompt::build` (`prompt.rs:50-69`) + fresh `list_memory()` (`chat.rs:1053`, `ORDER BY id` `storage.rs:524-528`, "read fresh every turn" `chat.rs:1043-1046`) + summary growing (`chat.rs:1058-1061`) + skill block (`chat.rs:1047`). Window slide (`chat.rs:1062-1063`, `summarized_upto` maju pasca-compaction `:1017-1019`). Setiap `remember`/skill/summary edit → byte message[0] geser → full-prefix miss |
| L2 breakpoint | ABSENT | `grep cache_control\|prompt_cache_key\|retention` di `src/` = **nol**. Wire OpenAI-only, tidak ada konsep breakpoint |
| L3 affinity | PARTIAL | `session_id` sticky stabil (`ChatRequest.session_id` `mod.rs:140-145`, dipakai tiap chat/stream/compaction/subagent) tapi **Orvix-gated**: `resolve_session_id` error bila `send_session_id` (`openai.rs:53-68`), default `false` (`chat.rs:333`, `config.rs:42-45`). Backend generik kirim **tanpa identitas stabil sama sekali**. Tanpa `prompt_cache_key`/header |
| L4 replay | PRESENT (parsial) | Tool id/args round-trip verbatim storage→wire (`ToolCall{id,name,arguments: String}` `mod.rs:131-137`, persist `storage.rs:695-708`, reload `ORDER BY id` `:640-683`, test `openai.rs:~770`). Thinking N/A — tidak ada field, yang justru **menguntungkan** (satu sumber prefix-break hilang) |
| L5 hygiene | ABSENT | Tanpa konsep retention. `/model` switch pertahankan history, hanya ganti string `model` → miss pasti. `summary_request` shape ad-hoc (2 pesan, `tools: []` yang di-omit di wire `compaction.rs:71-88`), usage-nya dibuang (`chat.rs` hanya ambil `.content`). Title request segregate usage (`kind='title'`) tapi tetap same-model request terpisah |
| Observability | READ-ONLY | `cached_tokens` di-parse 4 varian (`mod.rs:197-207`), disimpan per turn (`storage.rs:708-715`), ditampilkan footer/sidebar/`/stats` — **tidak pernah ditindaklanjuti** (tanpa breakpoint/key/retention untuk di-tune) |

### Ranking prefix-breaker kamui (dampak terbesar dulu)

1. **message[0] rebuild** (`chat.rs:1047-1062`) — kepala request berubah tiap ada
   memory/summary/skill delta. Melanggar "kepala frozen".
2. **Roster flip** — plan-pending swap ke subset (`chat.rs:982-990`,
   `plan_mode_definitions` `:2923`), `search_code` kondisional (`:987-989`),
   `tools=false` profile kirim `[]` yang lalu **hilang dari JSON** via
   `skip_serializing_if` (`openai.rs:136-137,144-145`), MCP connect/disconnect
   ubah `extra` (`tools.rs:89`). Field `tools` muncul/hilang/berubah = suffix
   prefix break.
3. **Tanpa affinity key generik** — Orvix dapat `session_id`, backend lain nol.

## 4. Roadmap improvement (termurah dulu)

### P0 — tanpa file baru, tanpa API baru

1. **Kirim affinity key stabil (~5-10 baris).** Tambah `prompt_cache_key` (atau
   field ekuivalen yang di-honor backend) = stable `coding_session_id` di
   `OpenAIRequest`/`OpenAIStreamRequest` (`openai.rs:133-148`), di samping
   `session_id`. Caveat: `prompt_cache_key` adalah param Responses API —
   **verifikasi dulu apakah backend (Orvix/generik) meng-honor-nya di
   `/chat/completions`**; bila tidak, alternatifnya pastikan body stabil (P0.2,
   P0.3) karena chat-completions caching murni prefix-match server-side.
2. **Satu tool array fixed per session.** Hoist `tool_definitions` keluar dari
   branch plan-mode (`chat.rs:982-994`); gate Plan Mode via teks prompt, bukan
   roster surgery; putuskan `search_code` sekali per session. Catat batas:
   profile `tools=false` memang harus omit field (endpoint reject) — stabilitas
   hanya dijamin **dalam session yang sama**, bukan lintas profile.
3. **Freeze kepala prefix.** Pecah message[0] (`chat.rs:1048-1062`) jadi pesan
   terpisah bertemplate stabil: system base sekali, blok memory + summary
   sebagai pesan sendiri; cache `list_memory()` per session, refresh hanya
   pasca memory-tool call (bukan `read fresh every turn`). Setiap fact/summary
   edit berhenti menggeser seluruh prefix.

### P1 — observability jadi aksi

4. **Cache-miss detection ala `cache-stats.ts`.** Hari ini `cached_tokens`
   read-only. Bandingkan `cached_tokens` antar turn dengan threshold noise
   (Pi: 1024 token) + label sebab (model switch / idle>TTL / roster flip) di
   footer yang sudah ada. Tanpa ini, P0 tidak terukur.
5. **Stabilkan summary template.** `summary_request` (`compaction.rs:71-88`)
   instruction string frozen (jangan edit tanpa sadar — tiap edit = baseline
   baru); pertimbangkan retention-none ekuivalen: pisahkan usage compaction
   dari stats chat (sebagian sudah: `kind` filter) agar request sampingan tidak
   mengotori sinyal.

### P2 — butuh adapter/API baru (tunda sampai demand)

6. **Anthropic native adapter** (`src/provider/anthropic.rs`, ≈8 SP, SPRINT_PLAN
   C2) — prasyarat L2 `cache_control` breakpoints + `thinking` replay. Lihat
   `docs/research/anthropic-native-adapter-vs-shim.md`: shim cukup untuk
   unblock Claude, native hanya wajib saat caching/thinking jadi first-class.
   Sampai ada benchmark/demand, P0–P1 adalah jawaban malas yang benar.

## 5. Referensi

- `docs/research/pi-cache-hit-research.md` — riset asli (clone, bukan pindahan).
- Pi-mono live: `packages/ai/src/api/{anthropic-messages,openai-responses,openai-completions,openai-prompt-cache,transform-messages}.ts`,
  `packages/coding-agent/src/core/cache-stats.ts`, `packages/coding-agent/docs/models.md` (~395-495).
- Kamui: `src/provider/{mod.rs,openai.rs}`, `src/chat.rs` (turn assembly
  `:982-1070`, compaction `:996-1039`, `:1943-1957`), `src/compaction.rs`,
  `src/tools.rs` (`with_defaults` `:57-91`, `definitions` `:154-161`),
  `src/storage.rs`, `src/prompt.rs`, `src/config.rs:42-45`.

## 6. Analogi simpel

- **Prefix cache = nasi di warteg.** Kamu pesan nasi yang sama tiap hari.
  Tukang masak simpan nasi tetap hangat. Kalau kamu ganti jenis beras,
  ia masak ulang dari nol. Kalau kamu hanya tambah lauk baru, nasi tetap hangat.
  Provider perlakukan awal request seperti nasi itu: sama = hangat (hit),
  beda 1 byte = masak ulang (miss).
- **message[0] rebuild = papan nama warung ditulis ulang tiap hari.**
  Kemarin "Warteg Bu Tuti", hari ini "Warteg Bu Tuti (ada info baru)".
  Pelanggan (cache) tidak kenal lagi, padahal isinya sama.
  Itu yang terjadi di `chat.rs:1048-1062`: memory + summary + skill
  ditempel ulang ke system message setiap Turn.
- **Roster flip = menu berubah tiap kunjungan.** Dapur (cache) mulai dari nol
  setiap menu beda. Plan Mode menukar ToolRegistry (`chat.rs:982-990`),
  sama seperti ganti menu di tengah makan.
- **Affinity key = kartu pelanggan / nomor meja.** Pelayan tahu kamu duduk
  di meja yang sama, jadi pesanan lama tetap berlaku. Kamui hanya kasih
  nomor meja ke Orvix (`session_id`, `openai.rs:53-68`); ke backend lain
  tidak kasih apa-apa.
- **Breakpoint = pembatas buku.** Pi taruh 3 pembatas (system, tools, pesan
  terakhir) supaya provider tahu bagian mana yang disimpan. Kamui tidak
  punya pembatas sama sekali (wire OpenAI-only).
- **Hygiene = jangan coret buku perpustakaan.** Request sampingan
  (Compaction, title) tidak boleh kotori cache utama. Pi paksa
  `cacheRetention:none` untuk itu; kamui belum punya konsepnya.

## 7. Sisa gap pasca-P0/P1 (2026-09-04, commit `7bc063b`)

- **`prompt_cache_key` belum tentu di-honor backend.** Field ini param
  Responses API; efeknya di `/chat/completions` tergantung backend
  (Orvix/generik) meneruskan ke prefix-cache atau mengabaikan unknown field.
  Yang pasti bekerja terlepas dari itu: stabilitas body P0.2/P0.3 (murni
  prefix-match server-side). Verifikasi empiris: bandingkan `cached_tokens`
  sebelum/sesudah via label P1.4 dalam sesi nyata.
- **L2 Anthropic tidak applicable** tanpa native adapter (P2.6, deferred).
- **Prefix-breaker by design yang tersisa:** Compaction majukan
  `summarized_upto`, MCP connect/disconnect ubah `extra`, ganti profile/model.
