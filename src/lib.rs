//! AI Session Board — real-time dashboard for Claude Code & Codex CLI.
//!
//! Two render targets:
//!   - `widget` — Win32 GDI floating overlay (always-on-top window)
//!   - `tui`    — ratatui TUI rendered in the current terminal
//!
//! The board reads:
//!   - `tu session -j` (subprocess) for session list, tokens, cost
//!   - `~/.claude/projects/**/*.jsonl` for accurate RUN time via mtime
//!   - `%LOCALAPPDATA%\tokenusage\live-frame-cache.json` for OAuth quota
//!   - System processes (sysinfo) for CPU/RAM charts of `claude*` / `codex*`

pub mod widget;
pub mod tui;

/// Launch the Win32 floating overlay (Windows-only).
pub fn run_floating() { widget::run(); }

/// Launch the ratatui TUI in the current terminal.
pub fn run_tui() -> std::io::Result<()> { tui::run() }
