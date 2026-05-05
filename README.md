# AI Session Board

Real-time dashboard for **Claude Code** and **Codex CLI** sessions on Windows.

Two render targets in one binary:
- **Floating Win32 overlay** — always-on-top, draggable, never steals keyboard focus
- **In-terminal TUI** — pin it to a tmux/psmux pane

Shows per-project session activity, accurate task-run duration, OAuth quota usage, month-to-date cost, and live CPU/RAM charts for `claude*` and `codex*` processes.

```
■ AI SESSION BOARD ■                                                  09:12:58
SRC     PROJECT          MODEL    RUN     LAST    TOKENS   COST     ●
CLAUDE  code/psmux       OPS4.7   3m      今      48.9M    28.10    ●
CLAUDE  code/dragonfly   SNT4.6   1m      1m      29.1M    13.51    ●
CLAUDE  code/openrouter  OPS4.7   32s     今      27.9M    20.58    ●
CLAUDE  mike2            SNT4.6   0s      3m       6.5M     2.54    ●
┌─ CPU %  ●claude 12.3%  ●codex 0.5% ─┐ ┌─ RAM %  ●claude 8.7%  ●codex 1.2% ─┐
│                                      │ │                                     │
│       /\        /\                   │ │ ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ │
│  ____/  \______/  \________          │ │                                     │
└──────────────────────────────────────┘ └─────────────────────────────────────┘
CC 11%/wk42%  CDX 0%/wk98%  RST 3h56m  今$222  MTD$1.3K
```

## Install

```bash
cargo install ai-session-board
```

Or clone and build:

```bash
git clone https://github.com/warmbed/ai-session-board
cd ai-session-board
cargo build --release
# Binary: target/release/ai-board.exe
```

## Usage

```bash
ai-board              # Floating Win32 overlay
ai-board --tui        # ratatui TUI in current terminal
ai-board -t           # short for --tui
ai-board -h           # help
```

### TUI controls

| Key | Action |
|---|---|
| `q` / `Esc` / `Ctrl+C` | quit |
| `a` | toggle 24H / Active Only filter |
| `c` | toggle CPU/RAM charts |
| `r` | force refresh |

### Floating window

- Drag anywhere on the window to move
- Right-click for context menu:
  - Toggle 24H / Active Only
  - Charts on / off
  - Opacity (40% / 70% / 100%)
  - Brightness (80% / 100% / 130% / 160%)
  - Size (50% / 75% / 100% / 150% / 200%)
  - Close

## Data sources

The board does **not** require any running server. It reads:

| Source | Purpose |
|---|---|
| `tu session -j --since YYYYMMDD` (subprocess) | session list, tokens, cost, models |
| `~/.claude/projects/**/*.jsonl` | last user-message timestamp + file mtime → accurate RUN duration |
| `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (via `tu`) | Codex sessions |
| `%LOCALAPPDATA%\tokenusage\live-frame-cache.json` | quota footer (CC/CDX %) |
| `sysinfo` crate | CPU/RAM for processes whose name contains `claude` / `codex` |

### Why JSONL mtime?

`tu session` records when each message was *received*, but a Claude task can stream tokens for minutes after the response begins. The file's mtime keeps advancing as bytes are written, so:

```
RUN = file_mtime - timestamp_of_last_real_user_message
```

gives the actual cook duration ("Cooked for 10m 26s" semantics), not just the gap between user-prompt and first response chunk.

`type:"user"` JSONL entries that are tool results are filtered out — only real user typing resets `run_start`.

## Requirements

- Windows 10 / 11 (Win32 floating overlay is Windows-only; TUI works anywhere a terminal does)
- [`tu`](https://github.com/...) CLI in `PATH` for session data and quota
- Rust 1.74+ to build from source

## Architecture

```
src/
├── main.rs     # CLI dispatch (--tui flag)
├── lib.rs      # public API: run_floating(), run_tui()
├── widget.rs   # Win32 GDI floating overlay
└── tui.rs      # ratatui TUI + sysinfo sampler thread
```

Session data fetching is duplicated between widget.rs and tui.rs for now; consolidating into a shared `data` module is on the roadmap.

## License

MIT — see [LICENSE](LICENSE).
