# Ringkasan Arsitektur Kamui

## Gambaran umum

Kamui adalah coding agent terminal berbasis Rust yang provider-agnostic dan sadar repository.
Konfigurasi, runtime agent, tool, terminal, indexing, job queue, dan persistence dipisahkan agar
fitur baru tidak membocorkan detail wire protocol provider ke core.

```text
CLI / terminal
  -> main.rs
     -> chat.rs -> provider/mod.rs -> provider/openai.rs
     -> context.rs / tools.rs
     -> storage.rs (SQLite)
     -> benchmark.rs
     -> jobs.rs
```

## Modul utama

### `src/main.rs`

Composition root dan parser CLI. Menangani chat interaktif, `-p`, `doctor`, `status`, benchmark, dan
subcommand persistent jobs. Konfigurasi berasal dari `kamui.toml` global dan project, bukan `.env`.

### `src/chat.rs`

Mengelola streaming chat, slash commands, approval, context compaction, semantic indexing, tool
loop, title session, persistence, dan cancellation. Beberapa `spawn_agent` independen dalam satu
response berjalan paralel dengan batas empat; sub-agent tetap hanya mendapat tool read-only.

### `src/provider/*`

`provider/mod.rs` berisi tipe request, response, message, usage, embedding, streaming, dan tool-call
yang netral. `provider/openai.rs` menjadi satu-satunya tempat untuk payload Chat Completions, SSE,
embeddings, dan parsing khusus OpenAI-compatible.

### `src/context.rs`

Memuat `KAMUI.md` atau `AGENTS.md`, mengembangkan `@file`, quoted path ber-spasi, directory context,
`@diff`, `@staged`, image, dan clipboard. Semua path dikunci ke project root dengan batas ukuran.

### `src/tools.rs`

Menyediakan registry dan tool read/list/search, plan, command, background command sementara, patch,
serta definisi pseudo-tool. `run_command` dan `patch_file` membutuhkan approval kecuali diizinkan
secara eksplisit. RTK hanya optimasi output dan selalu punya fallback direct execution.

### `src/storage.rs`

SQLite global dengan WAL, foreign keys, dan migrasi `PRAGMA user_version`. Menyimpan session,
message/tool trail, usage, memory, settings, scheduled jobs, serta index per project. Schema v9
menambahkan scheduled queue; v10 menambahkan embedding-model tracking, FTS5, dan LSH buckets.
Pergantian index per file dilakukan transaksional setelah seluruh embedding siap.

### `src/jobs.rs`

Persistent command queue untuk one-shot dan interval jobs. `kamui jobs worker` mengklaim due job
secara atomik, menjalankannya di working directory asal, menyimpan capped output dan exit code, serta
menghentikan job pada timeout 30 menit. Missed interval dikoales, bukan dibackfill.

### `src/benchmark.rs`

Menjalankan JSON benchmark suite pada profile tertentu. Tiap case dapat memiliki
`expect_contains`; laporan mencakup pass rate, latency, dan token, dengan exit non-zero saat gagal.

### `src/terminal.rs` dan `src/markdown.rs`

Terminal tetap line-oriented. Event tool menampilkan status, durasi, dan ukuran output; spinner hanya
aktif saat stdin/stdout adalah TTY. Pipe dan `-p` tetap plain, dan `NO_COLOR` menonaktifkan warna.
Markdown dirender per baris agar streaming tetap hidup.

## Semantic search

`/index` berjalan manual, menghormati ignore rules, memakai chunk sekitar 50 baris dengan batas
deklarasi/blank-line, dan mengirim embedding dalam batch. File yang tidak berubah dilewati hanya
jika content hash dan embedding model sama.

Untuk project kecil, pencarian menjalankan exact cosine pada semua chunk. Untuk project besar,
SQLite FTS5 menghasilkan kandidat identifier/path dan LSH menghasilkan kandidat vector-nearby;
exact cosine hanya mererank union yang dibatasi. Index selalu scoped menggunakan canonical project
root.

## Alur request utama

1. `main.rs` memuat config, database, project context, provider, MCP tools, dan tool registry.
2. `chat.rs` membangun system prompt, memory snapshot, history, dan attachment request.
3. Provider mengalirkan response; terminal merender delta tanpa mengubah data yang disimpan.
4. Tool call dijalankan setelah approval bila perlu; independent read-only sub-agents dapat paralel.
5. Turn lengkap disimpan atomik setelah jawaban final. Turn terputus tidak disimpan dan patch parsial
   dikembalikan melalui snapshot.

## Batas keamanan

- File dan symlink tidak boleh keluar dari project root.
- Command mutation membutuhkan approval; unattended `-p` menolak mutation secara default.
- Scheduled command dibuat eksplisit lewat CLI dan hanya berjalan ketika worker lokal aktif.
- Sub-agent tidak dapat menjalankan command, mengedit, menyentuh memory, atau spawn ulang.
- Tool loop, command runtime, context size, embedding batch, dan output memiliki batas.
- API key hanya boleh berada di config global.

## Dependency utama

- `tokio` untuk async runtime dan subprocess.
- `reqwest` untuk HTTP/SSE.
- `serde`, `serde_json`, dan `toml` untuk data/config.
- `rusqlite` bundled untuk persistence, FTS5, dan queue.
- `futures-util` untuk bounded concurrent sub-agent batches.
- `ignore`, `globset`, dan `regex` untuk repository search/context.
- `anyhow` dan `async-trait` untuk error handling dan async abstractions.
