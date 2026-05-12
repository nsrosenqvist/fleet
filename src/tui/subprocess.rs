//! Suspend / resume the ratatui-managed terminal around a foreground child
//! process that wants its own TTY (tmux attach, `$EDITOR`, `gh dash`, …).
//!
//! Pattern:
//!   1. Disable mouse capture + raw mode, leave alternate screen → terminal
//!      returns to its normal scrollback buffer, child gets a clean TTY
//!      without our SGR mouse-tracking escape sequences bleeding into its
//!      input stream.
//!   2. Spawn the child, wait for it to exit.
//!   3. Re-enter alternate screen + re-enable raw mode + re-enable mouse
//!      capture + force redraw.

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::{execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;

pub fn suspend_around<F, T>(term: &mut Terminal<CrosstermBackend<io::Stdout>>, body: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    // Leave the TUI's terminal state.
    execute!(
        io::stdout(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    terminal::disable_raw_mode()?;

    let result = body();

    // Re-enter. Even if the body errored, we must restore the terminal or
    // the user is left in a half-broken state.
    terminal::enable_raw_mode().ok();
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )
    .ok();
    term.clear().ok();

    result
}
