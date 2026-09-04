# Kamui TUI/UX — Audit & Roadmap

Dokumen kerja untuk mengembangkan fullscreen TUI agar **lebih percaya diri dipakai**, **lebih smooth**, dan **tidak terasa flat**.  
Branch kerja: **`main`**. Bukan product roadmap fitur AI — fokus UI/UX terminal.

Sumber fakta: `src/ui.rs`, `src/chat.rs` (ask_user / permission), `src/tui.rs`, `docs/ui-architecture.md`, `ROADMAP.md` Phase 7, plus audit kode Maret–September 2026.

---

## 1. Ringkasan eksekutif

**Verdict:** Fondasi opencode-style sudah kuat (InputHub, thick rails, fold tools, bouncing wall, permission modal, steering). Ambang “demo Ratatui” sudah lewat.

Yang masih jelek menurut pemakaian nyata:

1. **Trust interaksi** — permission hanya panah+Enter (user bingung); `y/a/n` tidak hidup; `ask_user` masih menulis ke stdout di alternate screen; Up/Down bukan line motion; footer hint bohong (`End` vs `Ctrl+End`) dan hampir tak terbaca.
2. **Tampilan flat** — near-black + muted rails + chrome abu membuat transcript, sidebar, editor, dan modal terasa **satu lapisan**. Kurang kedalaman surface, kurang hierarki peran, kurang fokus “apa yang aktif sekarang”.
3. **Smoothness** — redraw thinking 50ms + clone `Model` penuh; docs drift (spinner di footer vs wall di editor).

**Arah:** perbaiki kepercayaan + anti-flat visual hierarchy + biaya redraw.  
**Bukan arah:** Electron/GUI, theme engine penuh, clone opencode 1:1, plugin skin.

---

## 2. Arsitektur sekarang (fakta kode)

### Dua surface

| Mode | Kapan | Renderer |
|------|--------|----------|
| Fullscreen TUI | TTY interaktif | Ratatui di `src/ui.rs` (~4.5k LOC) |
| Plain | `-p`, pipe, `NO_COLOR`, non-TTY | `src/render.rs` + markdown ANSI |

### Layout aktual

```
┌ transcript (thick rails ▌) ─────────┐ ┌ sidebar (BG_PANEL) ┐
│ cards: user / assistant / tool / …  │ │ Session, Model, …  │
│ short stack padded ke bawah         │ │ Context, Last turn │
├─────────────────────────────────────┤ └────────────────────┘
│ slash menu / search (popup)         │
├─────────────────────────────────────┤
│ editor (BG_ELEMENT) + bouncing wall │
├─────────────────────────────────────┤
│ footer: ? help · hints · tok badge  │
└─────────────────────────────────────┘
  overlays: permission → help → dialog
```

Catatan: `docs/ui-architecture.md` masih menyebut meta version/model/path **di bawah editor** — itu **drift**. Meta ada di sidebar; editor = prompt + wall.

### InputHub

- Satu thread keyboard; `HubEvent::{Line, Quit}`.
- Busy: Enter → queue; Esc/Ctrl+C → interrupt `Notify`.
- Approval / ask_user: one-shot requester (bypass queue).
- Modal (Ctrl+K/S, `?`, search, permission) own keyboard sementara terbuka.

### Visual grammar (palette)

| Token | Hex | Peran |
|-------|-----|--------|
| TEXT | `#eeeeee` | Body |
| MUTED | `#808080` | Meta |
| BORDER | `#484848` | Chrome / footer (sering terlalu gelap) |
| BG_ELEMENT | `#1e1e1e` | Editor |
| BG_PANEL | `#101010`–`#141414` | Sidebar |
| POPUP_BG | `#0a0a0a` | Overlay |
| BLUE | `#5c9cf5` | Brand / assistant / wall |
| ACCENT | `#fab283` | User |
| GREEN / WARN / RED | outcome / token / error |

---

## 3. Audit interaksi (severity)

### P0 — harus diperbaiki dulu

| Issue | Detail | Target |
|-------|--------|--------|
| `ask_user` di TUI | `chat.rs` masih `println!`/`print!` ke stdout lalu `request_line`. Di alternate screen merusak frame; pertanyaan tidak jadi card/overlay. | Overlay / dialog bernomor seperti permission; **nol** raw stdout saat fullscreen. |

### P1 — trust & muscle memory

| Issue | Detail | Target |
|-------|--------|--------|
| Permission tanpa `y/a/n` | Modal menelan key tapi tidak map huruf; plain mode `[y/N/a]` sudah familiar. Label UI tidak menampilkan hotkey. | Map `y`/`a`/`n`; tampilkan di opsi; Enter tetap. |
| Footer bohong | `"End returns to live"` — End = EOL caret; live = **Ctrl+End**. | Perbaiki copy. |
| Footer contrast | Hint pakai `BORDER` `#484848` — hampir invisible. | Naik ke `MUTED` / dim TEXT. |
| Multiline Up/Down | Up/Down = history / slash menu, bukan baris buffer. | Up/Down = line motion jika multiline; history di first/last line atau Alt+Up (pilih satu, dokumentasikan di `?`). |

### P2 — polish interaksi

| Issue | Detail |
|-------|--------|
| Help height fixed 26 | Terminal pendek memotong keymap tanpa scroll. |
| Dialog vs slash filter | Dialog substring; slash prefix — inkonsisten. |
| Notice + modal | Path approval bisa redundant. |
| README spinner | Masih bilang spinner di footer fullscreen; realita wall di editor. |

---

## 4. Audit tampilan — kenapa terasa flat

### Diagnosis

1. **Surface hampir sewarna** — transcript (default bg), sidebar `#10/#14`, editor `#1e`, popup `#0a` terlalu dekat di terminal nyata; mata tidak membaca lapisan.
2. **Chrome lemah** — footer & hint terlalu gelap; tidak ada “active chrome” yang jelas saat modal/permission terbuka.
3. **Role hierarchy tipis** — thick rail bagus, tapi title/body/spacing antar card seragam; Note/command cells hampir hilang di laut abu.
4. **Modal tidak “naik”** — Clear + border orange cukup, tapi tanpa dim/backdrop yang konsisten + hotkey visible, terasa seperti kotak teks, bukan dialog.
5. **Sidebar airy berlebih** — blank line antar entry membuang tinggi; di terminal pendek terasa kosong + terpotong.
6. **Home vs chat** — logo besar lalu lompat ke chat flat; kurang transisi visual “session started”.

### Target anti-flat (tanpa jadi GUI)

| Prinsip | Artinya di TUI |
|---------|----------------|
| Layers | Transcript / sidebar / editor / overlay harus **terbaca berbeda** (bg + border + padding). |
| Focus | Editor saat idle/busy dan modal saat open = permukaan “depan”. |
| Hierarchy | User / assistant / tool / error punya bobot visual berbeda (rail, title bold, spacing). |
| Rhythm | Satu blank antar card; sidebar compact jika pendek; jangan void besar tanpa maksud. |
| Contrast | Meta terbaca; body lebih terang dari chrome. |
| Motion | Wall = presence; jangan 20fps clone penuh tanpa perlu. |

### Estetika yang **dilarang**

- Purple glow / indigo gradient “AI default”
- Pill cluster, emoji chrome, multi-shadow
- Card soup (border+radius+bg setiap bubble)
- Theme engine penuh sebelum Fase A–C selesai
- Port ke webview/Electron

---

## 5. Yang sudah bagus (jangan dirombak konsepnya)

- Thick rail tanpa background smear pada pesan
- Tool fold + outcome selalu terlihat
- Bouncing wall di editor (bukan spinner ganda di transcript)
- Steering + queue saat turn jalan
- Permission preview diff berwarna + scroll panjang
- Error fold + headline manusiawi; OpenAI `null` sequence ditoleransi
- Path sidebar left-truncate (`…/leaf`)
- Short transcript bottom-align di atas editor
- Plain mode tetap plain untuk pipe/`-p`

---

## 6. Roadmap bertahap

Kerja tetap di **`main`**. Setiap fase: kecil, bisa di-ship, ada DoD.

### Fase A — Trust (wajib)

**Tujuan:** user tidak bingung menjawab sistem; alt-screen tidak rusak.

| Item | File utama | DoD |
|------|------------|-----|
| `ask_user` overlay / dialog di TUI | `chat.rs`, `ui.rs` | Tidak ada `println` ke stdout saat fullscreen; pilihan 1–4 + free text; interrupt Esc tetap |
| Hotkey `y`/`a`/`n` + label di permission | `ui.rs` `input_thread`, `render_permission` | Sama semantik plain `[y/N/a]`; Enter/Esc tetap |
| Footer: `Ctrl+End` + warna `MUTED` | `ui.rs` `footer_widget` | Copy benar; terbaca di near-black |
| Multiline Up/Down line motion | `ui.rs` caret helpers + tests | Up/Down pindah baris; history hanya di tepi buffer / convention terdokumentasi |

**Out of scope Fase A:** theme, `@` popup, redesign logo.

### Fase B — Presence (anti-flat visual)

**Tujuan:** layar punya kedalaman dan fokus tanpa dekorasi murahan.

| Item | File utama | DoD |
|------|------------|-----|
| Bedakan surface sidebar / editor / overlay | palette + `sidebar_paragraph`, `editor_widget`, modal Clear | Di screenshot, tiga zona jelas berbeda |
| Modal elevation | `render_permission`, help, dialog | Backdrop dim konsisten; opsi + hotkey terbaca; fokus visual di dialog |
| Role hierarchy cards | `card_lines` | User/assistant/tool/error/note beda bobot; Note tidak hilang |
| Spacing rhythm | transcript + sidebar | Satu blank antar card; sidebar compact jika `height` kecil |
| Editor sebagai active surface | `editor_widget` + wall | Idle vs busy terasa beda fokus; wall tidak tabrakan placeholder |
| Footer readable + tenang | `footer_widget` | Chrome terbaca; tidak mengalahkan editor |

**Out of scope Fase B:** multi-theme picker, custom CSS-like config.

### Fase C — Smooth

**Tujuan:** terasa ringan dan konsisten saat session panjang.

| Item | File utama | DoD |
|------|------------|-----|
| Turunkan biaya thinking redraw | `thinking_start`, `draw`, wrap cache | Interval ≥100ms atau skip re-wrap; CPU turun terasa |
| Wall hint ellipsis di editor sempit | `editor_widget` | Tidak overflow kasar |
| Help overlay scrollable | `render_help` | Terminal pendek tetap navigable |
| `trim_history` by line budget | `FullScreen` / cards | Body raksasa tidak menahan 4k cards logic palsu |
| Sync docs | `docs/ui-architecture.md`, `README.md` | Layout/wall/footer sesuai kode |

### Fase D — Seamless + Later

**Seamless (masih in-scope product TUI):**

- Samakan algoritma filter dialog ↔ slash (prefix atau fuzzy — pilih satu).
- Error expand hint: `MUTED`/`WARN`, bukan hijau “sukses”.
- Cold-start: satu baris tease key (`?` · Ctrl+K · Ctrl+S) tanpa dump keymap.
- Catatan singkat di `ROADMAP.md` mengarah ke dokumen ini (tanpa mencentang deferred).

**Later (deferred — jangan mulai tanpa trigger user):**

| Item | Syarat boleh mulai |
|------|-------------------|
| Theme switching | Fase A–C selesai + permintaan user konkret |
| `@file` completion popup | Model completion + path safety didesain dulu |
| Timeline / subagent dialogs | Ada UX sub-agent yang butuh surface sendiri |

---

## 7. Prinsip desain (smooth & seamless)

1. **Zero frame corruption** — tidak ada `print!` ke stdout di path fullscreen.
2. **Muscle memory** — plain `[y/N/a]` ≈ TUI `y`/`a`/`n` + panah+Enter.
3. **One loading language** — wall di editor; transcript tidak menduplikasi spinner.
4. **Depth = layers & hierarchy**, bukan glow.
5. **Plain mode remains plain** — pipes dan `-p` tidak kena Ratatui.
6. **Smallest complete step** — ship fase A sebelum dekorasi besar.

---

## 8. Anti-goals

- GUI / mobile / voice / Kamui Dispatch di binary yang sama
- Rewrite `ui.rs` jadi framework komponen sebelum A–B selesai
- Theme marketplace / plugin skin
- Samakan 100% pixel dengan opencode
- Silent spend / animasi berat di startup

---

## 9. Urutan eksekusi yang disarankan

```text
A (Trust) ──┬──► B (Presence / anti-flat)
            └──► C (Smooth) partial overlap OK
                     └──► D seamless + deferred gates
```

Verifikasi tiap fase (jika ada perubahan Rust):

```sh
cargo fmt --all
cargo test
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

---

## 10. Checklist cepat “sudah tidak jelek?”

- [x] Permission: user bisa jawab dengan `y` / `a` / `n` tanpa baca novel
- [x] `ask_user` tidak merusak TUI
- [x] Multiline: panah atas/bawah pindah baris
- [x] Footer terbaca dan jujur
- [ ] Screenshot: sidebar ≠ transcript ≠ editor (lapisan jelas) — palette diangkat; verifikasi visual masih perlu mata manusia
- [x] Modal menampilkan hotkey; overlay lebih terang dari transcript
- [x] Thinking ticker 100ms (bukan 50ms)
- [x] Docs = perilaku nyata (layout / wall / ask_user)

---

*Dokumen ini hidup di `Plan/`. Update status fase dengan checklist di atas saat implementasi di `main`.*
