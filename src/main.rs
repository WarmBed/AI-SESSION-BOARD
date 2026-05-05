// Always use the "windows" subsystem so launching the binary never pops up a
// CMD console window, even in debug builds. The TUI variant still works fine
// when run from an existing terminal because Windows passes the parent's
// stdin/stdout handles to a windows-subsystem process when one is available.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let want_tui = args.iter().any(|a| a == "--tui" || a == "-t");
    let want_help = args.iter().any(|a| a == "--help" || a == "-h");

    if want_help {
        print_help();
        return;
    }

    if want_tui {
        if let Err(e) = ai_session_board::run_tui() {
            eprintln!("ai-board: {}", e);
            std::process::exit(1);
        }
    } else {
        ai_session_board::run_floating();
    }
}

fn print_help() {
    println!("AI Session Board — real-time dashboard for Claude Code & Codex CLI

USAGE:
  ai-board                Open as a floating, always-on-top Win32 overlay
  ai-board --tui          Render in the current terminal (works in psmux/tmux pane)
  ai-board -h, --help     Show this help

CONTROLS (TUI mode):
  q / Esc / Ctrl+C        quit
  a                       toggle 24H / Active Only filter
  c                       toggle CPU/RAM charts
  r                       force refresh

CONTROLS (Floating mode):
  Drag                    move window
  Right-click             context menu (24H toggle, charts, opacity, brightness, size, close)

DEPENDENCIES:
  Requires `tu` CLI in PATH (https://github.com/...) for session data and quota.
  Reads ~/.claude/projects/ and ~/.codex/sessions/ directly for accurate timing.
");
}
