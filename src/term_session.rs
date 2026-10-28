//! PTY session behind the integrated terminal, free of any UI types so it can
//! be driven headless in tests.
//!
//! Threading model (the point of this module — every piece here exists because
//! the old panel froze or ballooned under load):
//!
//! - **Reader thread** reads the PTY master and feeds the bytes straight into a
//!   VT emulator (`alacritty_terminal::Term`) under a fair mutex. There is no
//!   channel between the PTY and the UI, so a flooding process (`yes`,
//!   `seq 1 5000000`, `cat big.log`) cannot queue gigabytes in memory: once the
//!   parser is busy the reader stops reading, the kernel PTY buffer fills and
//!   the producer blocks — backpressure is the PTY itself. The emulator keeps
//!   a fixed scrollback, so memory is bounded no matter how much is printed.
//! - **The UI thread never parses.** It polls [`PtySession::take_dirty`] at
//!   frame rate and, when something changed, copies the visible rows out under
//!   the lock (`snapshot`) — cost proportional to rows shown, not to bytes
//!   received.
//! - **Writer thread** owns the PTY writer. The UI only appends to an input
//!   queue, so a child that stops reading stdin (a full tty input queue) can
//!   never block the UI on `write(2)`. [`PtySession::interrupt`] drops queued
//!   but unwritten input before sending ^C / ^Z / ^\ so a stuck paste cannot
//!   hold the signal hostage.
//! - **Shutdown never blocks the UI.** Killing (SIGHUP, grace period, SIGKILL)
//!   and reaping happen on a short-lived thread; the reader polls with a
//!   timeout so it exits even when an orphaned job keeps the PTY open.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{Config, TermMode, cell::Flags},
    vte::ansi::{self, Color, Handler as _, NamedColor},
};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// Lines of scrollback the emulator keeps by default (VS Code's default is
/// 1000; we keep more because Copy reads the whole buffer). Settings can
/// change it (`terminal_scrollback`) within [`MIN_SCROLLBACK_LINES`] ..
/// [`MAX_SCROLLBACK_LINES`]. Bounded: ~100 cols × 24 B/cell × 10k ≈ 24 MB
/// worst case per session; the oldest lines are dropped first.
pub const SCROLLBACK_LINES: usize = 10_000;
pub const MIN_SCROLLBACK_LINES: usize = 100;
pub const MAX_SCROLLBACK_LINES: usize = 100_000;

/// A scrollback setting clamped to what the emulator is allowed to keep.
pub fn clamp_scrollback(lines: usize) -> usize {
    lines.clamp(MIN_SCROLLBACK_LINES, MAX_SCROLLBACK_LINES)
}
/// Bytes per PTY read. Also bounds how long the reader holds the terminal
/// lock for one parse, which the UI thread may wait out for a frame: 16 KiB
/// of `yes` is 8k line feeds, ~3ms (64 KiB held it ~13ms, longer on a
/// loaded box).
const READ_CHUNK: usize = 16 * 1024;
/// Bytes per write(2) on the writer thread, so [`PtySession::interrupt`] can
/// cut into a long paste between chunks.
const WRITE_CHUNK: usize = 1024;
/// Most bytes per second the reader parses. A runaway producer (`yes`) is
/// held to this by PTY backpressure instead of pinning a core in the parser:
/// nobody reads 500 MB/s of output, and `cat` of a 40 MB log still takes
/// about a second.
const MAX_PARSE_BYTES_PER_SEC: u64 = 32 * 1024 * 1024;
/// Throttle accounting window.
const THROTTLE_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);
/// Output that keeps saturating the parse budget this long (`yes`, a runaway
/// log; `cat` of a big file is done well before) is parsed at
/// [`SUSTAINED_PARSE_BYTES_PER_SEC`]: the PTY backpressures the producer, so
/// a never-ending flood costs a sliver of a core instead of most of one.
const SUSTAINED_FLOOD_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
/// A throttle window with at least this much output counts as part of a
/// flood (1.25 MiB/s: far beyond any interactive program).
const FLOOD_BYTES_PER_WINDOW: u64 = 64 * 1024;
const SUSTAINED_PARSE_BYTES_PER_SEC: u64 = 8 * 1024 * 1024;
/// Line feeds cost far more than bytes (each one scrolls a full row of
/// cells), so `yes` is limited by lines too: bursts like `seq 5000000` run
/// unthrottled, a flood that never ends is cut to a rate the eye cannot
/// follow anyway.
const MAX_PARSE_LINES_PER_SEC: u64 = 4_000_000;
const SUSTAINED_PARSE_LINES_PER_SEC: u64 = 400_000;
/// After the shell process exits, how long to wait for EOF on the PTY before
/// declaring the session over anyway (a background job can hold it open).
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
/// How often a blocked reader wakes to check for shutdown.
const READER_POLL_MS: i32 = 100;

/// Terminal grid size in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub const DEFAULT: GridSize = GridSize {
        cols: 100,
        rows: 24,
    };

    pub fn clamped(cols: usize, rows: usize) -> GridSize {
        GridSize {
            cols: cols.clamp(20, 1000) as u16,
            rows: rows.clamp(4, 500) as u16,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Bytes waiting for the writer thread.
#[derive(Default)]
struct InputQueueState {
    pending: VecDeque<u8>,
    closed: bool,
}

#[derive(Default)]
struct InputQueue {
    state: Mutex<InputQueueState>,
    ready: Condvar,
}

impl InputQueue {
    fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            if state.closed {
                return;
            }
            state.pending.extend(bytes);
            self.ready.notify_one();
        }
    }

    /// Drop anything not yet written, then queue `bytes` (a control char).
    fn interrupt(&self, bytes: &[u8]) {
        if let Ok(mut state) = self.state.lock() {
            if state.closed {
                return;
            }
            state.pending.clear();
            state.pending.extend(bytes);
            self.ready.notify_one();
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.pending.clear();
            self.ready.notify_all();
        }
    }

    #[cfg(all(test, unix))]
    fn pending_len(&self) -> usize {
        self.state.lock().map(|s| s.pending.len()).unwrap_or(0)
    }

    /// Block until bytes are queued; `None` once closed.
    fn next_chunk(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().ok()?;
        loop {
            if state.closed {
                return None;
            }
            if !state.pending.is_empty() {
                let n = state.pending.len().min(WRITE_CHUNK);
                return Some(state.pending.drain(..n).collect());
            }
            state = self.ready.wait(state).ok()?;
        }
    }
}

/// Emulator events: replies the terminal must send back to the program
/// (device attributes, cursor position reports) go through the input queue.
#[derive(Clone)]
struct Listener {
    input: Arc<InputQueue>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.input.push(text.as_bytes());
        }
    }
}

struct Shared {
    term: FairMutex<Term<Listener>>,
    /// Set by the reader after every parse; cleared by the UI on snapshot.
    dirty: AtomicBool,
    /// Reader saw EOF / EIO (every slave handle closed).
    eof: AtomicBool,
    /// Asks the reader to exit at its next poll.
    stop: AtomicBool,
    bytes_read: AtomicU64,
    /// Set by an interrupt (Ctrl+C/Z/\\): the reader drops its flood
    /// throttling so the backlog after the signal drains at full speed and
    /// the screen settles on the prompt at once.
    interrupted: AtomicBool,
}

/// What the panel paints: plain rows (no colors yet) plus cursor and modes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub lines: Vec<String>,
    /// `(row in lines, column in chars)` when the program shows a cursor in
    /// the alternate screen. The normal screen uses the line composer instead.
    pub cursor: Option<(usize, usize)>,
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
}

/// Shape of the view without copying any text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewInfo {
    pub line_count: usize,
    /// `(view row, column in chars)` when the program shows a cursor.
    pub cursor: Option<(usize, usize)>,
    pub alt_screen: bool,
    pub app_cursor: bool,
}

/// A cell color, independent of any UI toolkit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellColor {
    /// The terminal's default foreground / background (follows the theme).
    Default,
    /// One of the 256 xterm palette entries (0–15 follow the theme).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Look of a run of cells that differs from the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
}

impl CellStyle {
    const PLAIN: CellStyle = CellStyle {
        fg: CellColor::Default,
        bg: CellColor::Default,
        bold: false,
        italic: false,
        underline: false,
        dim: false,
    };
}

/// A row's text plus its non-default style runs (byte ranges into `text`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledRow {
    pub text: String,
    pub runs: Vec<(std::ops::Range<usize>, CellStyle)>,
}

fn cell_color(color: Color) -> CellColor {
    match color {
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(ix) => CellColor::Indexed(ix),
        Color::Named(named) => match named {
            NamedColor::Black | NamedColor::DimBlack => CellColor::Indexed(0),
            NamedColor::Red | NamedColor::DimRed => CellColor::Indexed(1),
            NamedColor::Green | NamedColor::DimGreen => CellColor::Indexed(2),
            NamedColor::Yellow | NamedColor::DimYellow => CellColor::Indexed(3),
            NamedColor::Blue | NamedColor::DimBlue => CellColor::Indexed(4),
            NamedColor::Magenta | NamedColor::DimMagenta => CellColor::Indexed(5),
            NamedColor::Cyan | NamedColor::DimCyan => CellColor::Indexed(6),
            NamedColor::White | NamedColor::DimWhite => CellColor::Indexed(7),
            NamedColor::BrightBlack => CellColor::Indexed(8),
            NamedColor::BrightRed => CellColor::Indexed(9),
            NamedColor::BrightGreen => CellColor::Indexed(10),
            NamedColor::BrightYellow => CellColor::Indexed(11),
            NamedColor::BrightBlue => CellColor::Indexed(12),
            NamedColor::BrightMagenta => CellColor::Indexed(13),
            NamedColor::BrightCyan => CellColor::Indexed(14),
            NamedColor::BrightWhite => CellColor::Indexed(15),
            _ => CellColor::Default,
        },
    }
}

fn cell_style(cell: &alacritty_terminal::term::cell::Cell) -> CellStyle {
    let mut fg = cell_color(cell.fg);
    let mut bg = cell_color(cell.bg);
    let flags = cell.flags;
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
        // Inverse of the defaults: the UI swaps theme fg/bg for these.
        if fg == CellColor::Default && bg == CellColor::Default {
            return inverse_defaults(flags);
        }
    }
    CellStyle {
        fg,
        bg,
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        dim: flags.contains(Flags::DIM),
    }
}

/// Marker style for "inverse video on default colors" (reverse-video status
/// lines in less / top / vim). `fg == bg == Rgb(0,0,0)` with `underline`
/// unset is otherwise invisible text, so the UI reads it as "swap theme
/// foreground and background".
fn inverse_defaults(flags: Flags) -> CellStyle {
    CellStyle {
        fg: CellColor::Rgb(0, 0, 0),
        bg: CellColor::Rgb(0, 0, 0),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        dim: flags.contains(Flags::DIM),
    }
}

impl CellStyle {
    /// See [`inverse_defaults`].
    pub fn is_inverse_default(&self) -> bool {
        self.fg == CellColor::Rgb(0, 0, 0) && self.bg == CellColor::Rgb(0, 0, 0)
    }
}

/// A live shell (or any command) on a PTY.
pub struct PtySession {
    shared: Arc<Shared>,
    input: Arc<InputQueue>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    child_pid: Option<u32>,
    child_exited_at: Option<std::time::Instant>,
    size: GridSize,
}

impl PtySession {
    /// Spawn `cmd` on a new PTY of `size` with the default scrollback.
    #[cfg(test)]
    pub fn spawn(cmd: CommandBuilder, size: GridSize) -> Result<PtySession, String> {
        Self::spawn_with(cmd, size, SCROLLBACK_LINES)
    }

    /// Spawn `cmd` keeping `scrollback` lines of history (clamped).
    pub fn spawn_with(
        cmd: CommandBuilder,
        size: GridSize,
        scrollback: usize,
    ) -> Result<PtySession, String> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(pty_size(size))
            .map_err(|err| format!("PTY error: {err}"))?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| format!("Couldn’t start shell: {err}"))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| format!("Couldn’t read shell output: {err}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| format!("Couldn’t write to shell: {err}"))?;

        let input = Arc::new(InputQueue::default());
        let config = term_config(scrollback);
        let term = Term::new(
            config,
            &size,
            Listener {
                input: input.clone(),
            },
        );
        let shared = Arc::new(Shared {
            term: FairMutex::new(term),
            dirty: AtomicBool::new(true),
            eof: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
        });

        #[cfg(unix)]
        let poll_fd = pair.master.as_raw_fd();
        #[cfg(not(unix))]
        let poll_fd: Option<i32> = None;
        {
            let shared = shared.clone();
            thread::Builder::new()
                .name("malgel-pty-reader".into())
                .spawn(move || reader_loop(reader, poll_fd, shared))
                .map_err(|err| format!("Couldn’t start reader: {err}"))?;
        }
        {
            let input = input.clone();
            thread::Builder::new()
                .name("malgel-pty-writer".into())
                .spawn(move || writer_loop(writer, input))
                .map_err(|err| format!("Couldn’t start writer: {err}"))?;
        }

        let child_pid = child.process_id();
        Ok(PtySession {
            shared,
            input,
            master: Some(pair.master),
            child: Some(child),
            child_pid,
            child_exited_at: None,
            size,
        })
    }

    /// Queue bytes for the program. Never blocks on the PTY.
    pub fn write(&self, bytes: &[u8]) {
        self.input.push(bytes);
    }

    /// Paste text the way a terminal does: CR line endings, wrapped in
    /// bracketed-paste markers when the program asked for them.
    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.input.push(&paste_payload(text, bracketed));
    }

    /// The terminal lock, as the UI thread takes it: the wait is reported to
    /// MALGEL_PERF (`term_lock_wait` in the ui_lag line).
    fn ui_lock(&self) -> impl std::ops::DerefMut<Target = Term<Listener>> + '_ {
        let started = std::time::Instant::now();
        let guard = self.shared.term.lock();
        crate::perf::note_term_lock_wait(started.elapsed());
        guard
    }

    /// Send a signal character (^C, ^Z, ^\) ahead of any queued input.
    pub fn interrupt(&self, byte: u8) {
        self.shared.interrupted.store(true, Ordering::Release);
        self.input.interrupt(&[byte]);
    }

    /// Bytes queued for the program but not yet written.
    #[cfg(all(test, unix))]
    pub fn pending_input(&self) -> usize {
        self.input.pending_len()
    }

    pub fn bytes_read(&self) -> u64 {
        self.shared.bytes_read.load(Ordering::Relaxed)
    }

    /// Resize the PTY (the program gets SIGWINCH) and the emulator grid.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        if let Some(master) = &self.master {
            let _ = master.resize(pty_size(size));
        }
        self.ui_lock().resize(size);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// True (once) when output arrived since the last call.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// Whether the shell is gone: the PTY hit EOF or the shell process exited
    /// (a background job can keep the PTY open after `exit`).
    pub fn poll_exited(&mut self) -> bool {
        if self.shared.eof.load(Ordering::Acquire) {
            return true;
        }
        if self.child_exited_at.is_none()
            && let Some(child) = &mut self.child
            && let Ok(Some(_)) = child.try_wait()
        {
            self.child_exited_at = Some(std::time::Instant::now());
        }
        // The shell is gone but its last output may still be in flight to
        // the reader; give it a moment before calling the session over
        // (otherwise `echo hi; exit` loses "hi"). EOF wins when it comes.
        self.child_exited_at
            .is_some_and(|at| at.elapsed() >= EXIT_GRACE)
    }

    /// True when the shell has no foreground job (safe to restart).
    pub fn is_idle(&self) -> bool {
        #[cfg(unix)]
        {
            match (
                self.master.as_ref().and_then(|m| m.process_group_leader()),
                self.child_pid,
            ) {
                (Some(leader), Some(pid)) => leader as u32 == pid,
                _ => true,
            }
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    pub fn mode(&self) -> TermMode {
        *self.ui_lock().mode()
    }

    /// Change how many lines of history are kept; shrinking drops the
    /// oldest at once (and any selection that lived only there).
    pub fn set_scrollback(&self, lines: usize) {
        let mut term = self.ui_lock();
        term.set_options(term_config(lines));
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Start a selection at view `row`; `col` is a char boundary for
    /// [`SelectKind::Cells`], the char under the pointer otherwise. The
    /// selection lives in the emulator, anchored to buffer lines: output
    /// that scrolls or trims history moves it with its text.
    pub fn select_start(&self, kind: SelectKind, row: usize, col: usize) {
        let mut term = self.ui_lock();
        term_select_start(&mut term, kind, row, col);
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Move the selection's head (a drag) to view `row` / `col`.
    pub fn select_update(&self, row: usize, col: usize) {
        let mut term = self.ui_lock();
        term_select_update(&mut term, row, col);
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Select everything in the view.
    pub fn select_all(&self) {
        let mut term = self.ui_lock();
        term_select_all(&mut term);
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn clear_selection(&self) {
        let mut term = self.ui_lock();
        term.selection = None;
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// The selected text, soft-wrapped rows joined, trailing blanks trimmed.
    pub fn selection_text(&self) -> Option<String> {
        term_selection_text(&self.ui_lock())
    }

    /// The selection in view coordinates: start (row, char) inclusive to
    /// end (row, char) exclusive, `None` when nothing is selected.
    pub fn selection_view(&self) -> Option<((usize, usize), (usize, usize))> {
        term_selection_view(&self.ui_lock())
    }

    /// Visible rows: the alternate screen as-is, or for the normal screen the
    /// newest `max_lines` of scrollback + screen with trailing blank rows below
    /// the cursor dropped.
    pub fn snapshot(&self, max_lines: usize) -> Snapshot {
        let term = self.ui_lock();
        snapshot_term(&term, max_lines)
    }

    /// Row count, cursor and modes for the view; pair with [`Self::rows`],
    /// which the UI calls only for the rows actually on screen.
    pub fn view_info(&self) -> ViewInfo {
        let term = self.ui_lock();
        view_info_term(&term)
    }

    /// Text of view rows `range` (indices as in [`ViewInfo::line_count`]),
    /// clamped to what exists now.
    #[cfg(all(test, unix))]
    pub fn rows(&self, range: std::ops::Range<usize>) -> Vec<String> {
        let term = self.ui_lock();
        rows_term(&term, range)
    }

    /// [`Self::rows`] with colors and attributes.
    pub fn styled_rows(&self, range: std::ops::Range<usize>) -> Vec<StyledRow> {
        let term = self.ui_lock();
        let (top, bottom) = view_bounds(&term);
        let count = (bottom - top + 1).max(0) as usize;
        let end = range.end.min(count);
        let start = range.start.min(end);
        (start..end)
            .map(|ix| styled_row(&term, Line(top + ix as i32)))
            .collect()
    }

    /// Clear scrollback and screen, keeping the cursor's line at the top (VS
    /// Code's Clear keeps the prompt).
    pub fn clear(&self) {
        let mut term = self.ui_lock();
        let cursor = term.grid().cursor.point;
        let line = cursor.line.0.max(0) as usize;
        if line > 0 {
            term.scroll_up(line);
        }
        term.goto(0, cursor.column.0);
        term.clear_screen(ansi::ClearMode::Below);
        term.clear_screen(ansi::ClearMode::Saved);
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Print local status text (banners) into the emulator as if output.
    pub fn inject(&self, text: &str) {
        let mut processor: ansi::Processor = ansi::Processor::new();
        let normalized = text.replace('\n', "\r\n");
        let mut term = self.ui_lock();
        processor.advance(&mut *term, normalized.as_bytes());
        drop(term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Stop the session without blocking: close input, ask the reader to quit,
    /// and kill + reap the process tree on a helper thread.
    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.input.close();
        let child = self.child.take();
        #[cfg(unix)]
        let foreground = self.master.as_ref().and_then(|m| m.process_group_leader());
        let master = self.master.take();
        if child.is_none() && master.is_none() {
            return;
        }
        let _ = thread::Builder::new()
            .name("malgel-pty-reaper".into())
            .spawn(move || {
                #[cfg(unix)]
                if let Some(pgid) = foreground
                    && pgid > 0
                {
                    // The foreground job (e.g. `yes`) gets the hangup a real
                    // terminal sends when its window closes.
                    unsafe {
                        libc::kill(-pgid, libc::SIGHUP);
                    }
                }
                if let Some(mut child) = child {
                    // SIGHUP, ~200ms grace, then SIGKILL (portable-pty).
                    let _ = child.kill();
                    let _ = child.wait();
                }
                // Last handle on the master: closing it hangs up the slave
                // for anything still attached.
                drop(master);
            });
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The user's shell: `$SHELL`, else the login shell from the password
/// database (a desktop launcher or service may not export `SHELL`), else bash,
/// else `/bin/sh`. Falling straight to `/bin/sh` meant dash on Debian: no
/// line editing, so Tab, Up/Down history, Ctrl+R and Ctrl+L did nothing.
#[cfg(unix)]
pub fn user_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.trim().is_empty()
    {
        return shell;
    }
    // SAFETY: getpwuid returns a pointer into static storage (or null); the
    // string is copied out before any other passwd call.
    let from_passwd = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            None
        } else {
            std::ffi::CStr::from_ptr((*pw).pw_shell)
                .to_str()
                .ok()
                .map(str::to_owned)
        }
    };
    let usable = |p: &str| {
        !p.is_empty()
            && !p.ends_with("/nologin")
            && !p.ends_with("/false")
            && std::path::Path::new(p).exists()
    };
    if let Some(shell) = from_passwd.filter(|p| usable(p)) {
        return shell;
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/zsh", "/usr/bin/zsh"] {
        if usable(candidate) {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

/// Bytes a paste sends: CR line endings (what Enter sends), wrapped in
/// bracketed-paste markers when the program enabled them.
pub fn paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r");
    if !bracketed {
        return normalized.into_bytes();
    }
    // A pasted end marker would let the paste escape the bracket.
    let clean = normalized.replace("\x1b[201~", "");
    let mut payload = b"\x1b[200~".to_vec();
    payload.extend_from_slice(clean.as_bytes());
    payload.extend_from_slice(b"\x1b[201~");
    payload
}

fn pty_size(size: GridSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn reader_loop(mut reader: Box<dyn Read + Send>, poll_fd: Option<i32>, shared: Arc<Shared>) {
    let mut processor: ansi::Processor = ansi::Processor::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let budget = |rate: u64| rate * THROTTLE_WINDOW.as_millis() as u64 / 1000;
    let mut window_start = std::time::Instant::now();
    // Work done in the current window; overshoot carries into the next ones.
    let mut used_bytes = 0u64;
    let mut used_lines = 0u64;
    // Start of the run of busy windows (see FLOOD_BYTES_PER_WINDOW).
    let mut flood_since: Option<std::time::Instant> = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if shared.interrupted.swap(false, Ordering::AcqRel) {
            flood_since = None;
            used_bytes = 0;
            used_lines = 0;
            window_start = std::time::Instant::now();
        }
        let sustained = flood_since.is_some_and(|since| since.elapsed() >= SUSTAINED_FLOOD_AFTER);
        let (byte_budget, line_budget) = if sustained {
            (
                budget(SUSTAINED_PARSE_BYTES_PER_SEC),
                budget(SUSTAINED_PARSE_LINES_PER_SEC),
            )
        } else {
            (
                budget(MAX_PARSE_BYTES_PER_SEC),
                budget(MAX_PARSE_LINES_PER_SEC),
            )
        };
        if used_bytes >= byte_budget || used_lines >= line_budget {
            // Over budget: wait out the window (the PTY backpressures the
            // producer meanwhile), then pay the overshoot off.
            flood_since.get_or_insert_with(std::time::Instant::now);
            let elapsed = window_start.elapsed();
            if elapsed < THROTTLE_WINDOW {
                thread::sleep(THROTTLE_WINDOW - elapsed);
            }
            window_start = std::time::Instant::now();
            used_bytes = used_bytes.saturating_sub(byte_budget);
            used_lines = used_lines.saturating_sub(line_budget);
            continue;
        } else if window_start.elapsed() >= THROTTLE_WINDOW {
            if used_bytes >= FLOOD_BYTES_PER_WINDOW.min(byte_budget / 2)
                || used_lines >= line_budget / 2
            {
                flood_since.get_or_insert_with(std::time::Instant::now);
            } else {
                // A quiet window: the flood is over.
                flood_since = None;
            }
            window_start = std::time::Instant::now();
            used_bytes = 0;
            used_lines = 0;
        }
        if !wait_readable(poll_fd) {
            continue;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                {
                    let mut term = shared.term.lock();
                    processor.advance(&mut *term, &buf[..n]);
                }
                used_bytes += n as u64;
                used_lines += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
                shared.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
                shared.dirty.store(true, Ordering::Release);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    shared.eof.store(true, Ordering::Release);
    shared.dirty.store(true, Ordering::Release);
}

/// Wait up to [`READER_POLL_MS`] for the PTY to become readable so the reader
/// can notice shutdown. Returns true when a read will not block forever.
fn wait_readable(fd: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        let Some(fd) = fd else {
            return true;
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, READER_POLL_MS) };
        // Error / hangup: let read() report it.
        rc != 0
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, READER_POLL_MS);
        true
    }
}

fn writer_loop(mut writer: Box<dyn Write + Send>, input: Arc<InputQueue>) {
    while let Some(chunk) = input.next_chunk() {
        if writer.write_all(&chunk).is_err() {
            break;
        }
        let _ = writer.flush();
    }
    input.close();
}

/// Text of one grid row and whether it soft-wraps into the next.
fn row_text<T>(term: &Term<T>, line: Line) -> (String, bool) {
    let grid = term.grid();
    let row = &grid[line];
    let cols = grid.columns();
    let mut text = String::with_capacity(cols);
    for col in 0..cols {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra.iter());
        }
    }
    let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    (text, wrapped)
}

/// One grid row as text plus style runs; trailing blank default cells are
/// trimmed (a colored background keeps its cells).
fn styled_row<T>(term: &Term<T>, line: Line) -> StyledRow {
    let grid = term.grid();
    let row = &grid[line];
    let cols = grid.columns();
    let mut text = String::with_capacity(cols);
    let mut runs: Vec<(std::ops::Range<usize>, CellStyle)> = Vec::new();
    // Byte length of text up to the last cell worth keeping.
    let mut keep = 0;
    for col in 0..cols {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let start = text.len();
        text.push(cell.c);
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra.iter());
        }
        let end = text.len();
        let style = cell_style(cell);
        let visible_bg = style.bg != CellColor::Default || style.is_inverse_default();
        if cell.c != ' ' || visible_bg {
            keep = end;
        }
        if style == CellStyle::PLAIN {
            continue;
        }
        match runs.last_mut() {
            Some((range, last)) if *last == style && range.end == start => range.end = end,
            _ => runs.push((start..end, style)),
        }
    }
    text.truncate(keep);
    runs.retain_mut(|(range, _)| {
        range.end = range.end.min(keep);
        range.start < range.end
    });
    StyledRow { text, runs }
}

fn row_is_blank<T>(term: &Term<T>, line: Line) -> bool {
    let grid = term.grid();
    let row = &grid[line];
    (0..grid.columns()).all(|c| row[Column(c)].c == ' ')
}

/// Lowest line worth showing on the normal screen: the cursor line or the
/// last non-blank line below it.
fn last_content_line<T>(term: &Term<T>) -> i32 {
    let grid = term.grid();
    let cursor_line = grid.cursor.point.line.0;
    let mut bottom = grid.bottommost_line().0;
    while bottom > cursor_line && row_is_blank(term, Line(bottom)) {
        bottom -= 1;
    }
    bottom
}

/// First and last grid line of the view: the alternate screen as-is, or the
/// whole scrollback down to the cursor / last non-blank line.
fn view_bounds<T>(term: &Term<T>) -> (i32, i32) {
    let grid = term.grid();
    if term.mode().contains(TermMode::ALT_SCREEN) {
        (0, grid.bottommost_line().0)
    } else {
        (grid.topmost_line().0, last_content_line(term))
    }
}

fn view_info_term<T>(term: &Term<T>) -> ViewInfo {
    let mode = *term.mode();
    let grid = term.grid();
    let (top, bottom) = view_bounds(term);
    let cursor = mode.contains(TermMode::SHOW_CURSOR).then(|| {
        let Point { line, column } = grid.cursor.point;
        let row = (line.0 - top).max(0) as usize;
        // Column in chars: wide-char spacers are skipped in the row text.
        let gr = &grid[line];
        let col = (0..column.0.min(grid.columns()))
            .filter(|&c| {
                !gr[Column(c)]
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            })
            .count();
        (row, col)
    });
    ViewInfo {
        line_count: (bottom - top + 1).max(0) as usize,
        cursor,
        alt_screen: mode.contains(TermMode::ALT_SCREEN),
        app_cursor: mode.contains(TermMode::APP_CURSOR),
    }
}

fn rows_term<T>(term: &Term<T>, range: std::ops::Range<usize>) -> Vec<String> {
    let (top, bottom) = view_bounds(term);
    let count = (bottom - top + 1).max(0) as usize;
    let end = range.end.min(count);
    let start = range.start.min(end);
    (start..end)
        .map(|ix| {
            let (mut text, _) = row_text(term, Line(top + ix as i32));
            let trimmed = text.trim_end_matches(' ').len();
            text.truncate(trimmed);
            text
        })
        .collect()
}

fn snapshot_term<T>(term: &Term<T>, max_lines: usize) -> Snapshot {
    let info = view_info_term(term);
    let start = if info.alt_screen {
        0
    } else {
        info.line_count.saturating_sub(max_lines.max(1))
    };
    let lines = rows_term(term, start..info.line_count);
    Snapshot {
        lines,
        cursor: info
            .cursor
            .filter(|_| info.alt_screen)
            .and_then(|(r, c)| r.checked_sub(start).map(|r| (r, c))),
        alt_screen: info.alt_screen,
        app_cursor: info.app_cursor,
        bracketed_paste: term.mode().contains(TermMode::BRACKETED_PASTE),
    }
}

fn term_config(scrollback: usize) -> Config {
    Config {
        scrolling_history: clamp_scrollback(scrollback),
        ..Config::default()
    }
}

/// How a selection grows, as in VS Code: a drag selects cells, a
/// double-click words, a triple-click whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectKind {
    Cells,
    Word,
    Line,
}

impl SelectKind {
    /// Drag, double-click, triple-click (and beyond).
    pub fn from_click_count(count: usize) -> SelectKind {
        match count {
            0 | 1 => SelectKind::Cells,
            2 => SelectKind::Word,
            _ => SelectKind::Line,
        }
    }
}

fn is_spacer(flags: Flags) -> bool {
    flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

/// Grid column of char index `ch` on `line` (view text skips wide-char
/// spacers); `columns()` when past the end.
fn grid_col_of_char<T>(term: &Term<T>, line: Line, ch: usize) -> usize {
    let grid = term.grid();
    let row = &grid[line];
    let mut seen = 0;
    for col in 0..grid.columns() {
        if is_spacer(row[Column(col)].flags) {
            continue;
        }
        if seen == ch {
            return col;
        }
        seen += 1;
    }
    grid.columns()
}

/// Char index (in the view text) of grid column `col` on `line`.
fn char_of_grid_col<T>(term: &Term<T>, line: Line, col: usize) -> usize {
    let grid = term.grid();
    let row = &grid[line];
    (0..col.min(grid.columns()))
        .filter(|&c| !is_spacer(row[Column(c)].flags))
        .count()
}

/// Grid point and side for a view position (see [`PtySession::select_start`]).
fn view_point<T>(term: &Term<T>, kind: SelectKind, row: usize, col: usize) -> (Point, Side) {
    let (top, bottom) = view_bounds(term);
    let line = Line((top + row.min(i32::MAX as usize) as i32).min(bottom.max(top)));
    let columns = term.grid().columns();
    let gcol = grid_col_of_char(term, line, col);
    match kind {
        SelectKind::Cells if gcol >= columns => {
            (Point::new(line, Column(columns - 1)), Side::Right)
        }
        SelectKind::Cells => (Point::new(line, Column(gcol)), Side::Left),
        _ => (Point::new(line, Column(gcol.min(columns - 1))), Side::Left),
    }
}

fn term_select_start<T>(term: &mut Term<T>, kind: SelectKind, row: usize, col: usize) {
    let (point, side) = view_point(term, kind, row, col);
    let ty = match kind {
        SelectKind::Cells => SelectionType::Simple,
        SelectKind::Word => SelectionType::Semantic,
        SelectKind::Line => SelectionType::Lines,
    };
    term.selection = Some(Selection::new(ty, point, side));
}

fn term_select_update<T>(term: &mut Term<T>, row: usize, col: usize) {
    let Some(ty) = term.selection.as_ref().map(|s| s.ty) else {
        return;
    };
    let kind = match ty {
        SelectionType::Semantic => SelectKind::Word,
        SelectionType::Lines => SelectKind::Line,
        _ => SelectKind::Cells,
    };
    let (point, side) = view_point(term, kind, row, col);
    if let Some(selection) = term.selection.as_mut() {
        selection.update(point, side);
    }
}

fn term_select_all<T>(term: &mut Term<T>) {
    let (top, bottom) = view_bounds(term);
    let last = term.grid().columns() - 1;
    let mut selection = Selection::new(
        SelectionType::Simple,
        Point::new(Line(top), Column(0)),
        Side::Left,
    );
    selection.update(Point::new(Line(bottom), Column(last)), Side::Right);
    term.selection = Some(selection);
}

fn term_selection_text<T>(term: &Term<T>) -> Option<String> {
    let text = term.selection_to_string()?;
    // A triple-click copies the line, not the line and Enter (pasting it
    // back into a shell must not run it).
    let text = text.strip_suffix('\n').unwrap_or(&text);
    (!text.is_empty()).then(|| text.to_string())
}

fn term_selection_view<T>(term: &Term<T>) -> Option<((usize, usize), (usize, usize))> {
    let range = term.selection.as_ref()?.to_range(term)?;
    let (top, bottom) = view_bounds(term);
    if range.end.line < Line(top) || range.start.line > Line(bottom) {
        return None;
    }
    let (start, start_col) = if range.start.line < Line(top) {
        (Line(top), 0)
    } else {
        (
            range.start.line,
            char_of_grid_col(term, range.start.line, range.start.column.0),
        )
    };
    let (end, end_col) = if range.end.line > Line(bottom) {
        (Line(bottom), usize::MAX)
    } else {
        (
            range.end.line,
            char_of_grid_col(term, range.end.line, range.end.column.0) + 1,
        )
    };
    let row = |line: Line| (line.0 - top) as usize;
    Some(((row(start), start_col), (row(end), end_col)))
}

/// Bytes a key press sends to a program in raw (alternate-screen) mode, as
/// xterm encodes them. `key` is GPUI's key name, `key_char` the typed text.
pub fn encode_key(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    let csi_or_ss3 = |final_byte: u8| -> Vec<u8> {
        if app_cursor && !ctrl && !alt && !shift {
            vec![0x1b, b'O', final_byte]
        } else {
            let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
            if m == 1 {
                vec![0x1b, b'[', final_byte]
            } else {
                format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
            }
        }
    };
    let tilde = |n: u8| -> Vec<u8> { format!("\x1b[{n}~").into_bytes() };
    let mut bytes = match key {
        "enter" => vec![b'\r'],
        "backspace" => vec![0x7f],
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "escape" => vec![0x1b],
        "space" if ctrl => vec![0],
        "space" => vec![b' '],
        "up" => csi_or_ss3(b'A'),
        "down" => csi_or_ss3(b'B'),
        "right" => csi_or_ss3(b'C'),
        "left" => csi_or_ss3(b'D'),
        "home" => csi_or_ss3(b'H'),
        "end" => csi_or_ss3(b'F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        _ => {
            if ctrl {
                let mut chars = key.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                let byte = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    '/' => 0x1f,
                    '8' | '?' => 0x7f,
                    _ => return None,
                };
                vec![byte]
            } else if let Some(text) = key_char.filter(|t| !t.is_empty()) {
                text.as_bytes().to_vec()
            } else if key.chars().count() == 1 {
                if shift {
                    key.to_uppercase().into_bytes()
                } else {
                    key.as_bytes().to_vec()
                }
            } else {
                return None;
            }
        }
    };
    if alt && !bytes.starts_with(b"\x1b[") && !bytes.starts_with(b"\x1bO") {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

// The sessions under test are `/bin/sh` scripts on a Unix PTY.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> CommandBuilder {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(script);
        cmd.env("TERM", "xterm-256color");
        cmd
    }

    fn interactive_sh() -> CommandBuilder {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-i");
        cmd.env("TERM", "xterm-256color");
        cmd.env("PS1", "$ ");
        cmd.env("ENV", "/dev/null");
        cmd
    }

    fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    fn screen_text(session: &PtySession) -> String {
        session.snapshot(10_000).lines.join("\n")
    }

    fn rss_kib() -> u64 {
        crate::perf::rss_kib().unwrap_or(0)
    }

    #[test]
    fn exit_is_detected_and_output_kept() {
        let mut s = PtySession::spawn(sh("echo hello-pty; exit 3"), GridSize::DEFAULT).unwrap();
        assert!(wait_until(Duration::from_secs(10), || s.poll_exited()));
        assert!(screen_text(&s).contains("hello-pty"));
    }

    #[test]
    fn utf8_split_across_reads_is_not_mangled() {
        // printf in pieces: the 3-byte char straddles two writes.
        let mut s = PtySession::spawn(
            sh(r"printf '\344'; sleep 0.1; printf '\270\255-ok\n'"),
            GridSize::DEFAULT,
        )
        .unwrap();
        assert!(wait_until(Duration::from_secs(10), || s.poll_exited()));
        assert!(screen_text(&s).contains("中-ok"), "{}", screen_text(&s));
    }

    #[test]
    fn flood_is_parsed_off_the_ui_thread_with_bounded_memory() {
        // ~39 MB of output; memory must not track the byte count.
        let lines = 5_000_000u64;
        let before = rss_kib();
        let started = Instant::now();
        let mut s = PtySession::spawn(sh(&format!("seq 1 {lines}")), GridSize::DEFAULT).unwrap();
        let mut peak = before;
        // Simulated UI: snapshot at ~60 fps, never more than one snapshot of
        // work per frame regardless of how much arrived.
        let mut worst_snapshot = Duration::ZERO;
        while !s.poll_exited() {
            if s.take_dirty() {
                // What a frame costs the UI: shape + the rows on screen.
                let t = Instant::now();
                let info = s.view_info();
                let rows = s.rows(info.line_count.saturating_sub(60)..info.line_count);
                worst_snapshot = worst_snapshot.max(t.elapsed());
                assert!(rows.len() <= 60);
                assert!(info.line_count <= SCROLLBACK_LINES + GridSize::DEFAULT.rows as usize);
            }
            peak = peak.max(rss_kib());
            assert!(started.elapsed() < Duration::from_secs(120), "flood hung");
            thread::sleep(Duration::from_millis(16));
        }
        let secs = started.elapsed().as_secs_f64();
        let bytes = s.bytes_read();
        let mb_s = bytes as f64 / 1e6 / secs;
        let text = screen_text(&s);
        assert!(text.contains(&lines.to_string()), "tail missing");
        let grown_mb = peak.saturating_sub(before) as f64 / 1024.0;
        eprintln!(
            "flood: {:.1} MB in {secs:.2}s = {mb_s:.1} MB/s, rss +{grown_mb:.1} MiB, worst frame fetch {:.2}ms",
            bytes as f64 / 1e6,
            worst_snapshot.as_secs_f64() * 1000.0
        );
        assert!(bytes > 38_000_000);
        // Scrollback is capped: tens of MB of output, a few MB of grid.
        // (Process-wide RSS, shared with tests running in parallel: loose.)
        assert!(grown_mb < 256.0, "rss grew {grown_mb:.1} MiB");
        // Debug builds are slow, and after SUSTAINED_FLOOD_AFTER a flood of
        // short lines is held to SUSTAINED_PARSE_LINES_PER_SEC (~3 MB/s of
        // `seq`); release does far better (see report).
        assert!(mb_s > 1.0, "throughput {mb_s:.1} MB/s");
        assert!(worst_snapshot < Duration::from_millis(250));
    }

    #[test]
    fn ctrl_c_stops_a_flood_promptly() {
        let mut s = PtySession::spawn(interactive_sh(), GridSize::DEFAULT).unwrap();
        s.write(b"yes\r");
        assert!(wait_until(Duration::from_secs(10), || s.bytes_read() > 1_000_000));
        let t = Instant::now();
        s.interrupt(0x03);
        // Output must go quiet: bytes stop growing.
        let mut last = s.bytes_read();
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < Duration::from_millis(300) {
            thread::sleep(Duration::from_millis(20));
            let now = s.bytes_read();
            if now != last {
                last = now;
                quiet_since = Instant::now();
            }
            assert!(t.elapsed() < Duration::from_secs(5), "^C did not stop yes");
        }
        let stop_ms = (quiet_since - t).as_secs_f64() * 1000.0;
        eprintln!("ctrl-c: flood stopped {stop_ms:.0}ms after ^C");
        // Shell is alive and answers.
        s.write(b"echo after-int\r");
        assert!(wait_until(Duration::from_secs(5), || screen_text(&s)
            .contains("after-int\n")
            || screen_text(&s).lines().any(|l| l == "after-int")));
        assert!(!s.poll_exited());
    }

    #[test]
    fn writes_never_block_when_child_ignores_stdin() {
        // The child never reads: the tty input queue fills. A blocking write
        // on the UI thread used to hang the app here.
        let mut s = PtySession::spawn(sh("sleep 30"), GridSize::DEFAULT).unwrap();
        let big = vec![b'x'; 1 << 20];
        let t = Instant::now();
        for _ in 0..4 {
            s.write(&big);
        }
        assert!(t.elapsed() < Duration::from_millis(200), "write blocked");
        assert!(s.pending_input() > 0);
        // ^C jumps the queue and clears it.
        s.interrupt(0x03);
        assert!(s.pending_input() <= 1);
        assert!(wait_until(Duration::from_secs(10), || s.poll_exited()));
        let t = Instant::now();
        drop(s);
        assert!(t.elapsed() < Duration::from_millis(100), "drop blocked");
    }

    #[test]
    fn shutdown_is_non_blocking_and_kills_foreground_job() {
        let mut s = PtySession::spawn(interactive_sh(), GridSize::DEFAULT).unwrap();
        s.write(b"trap '' HUP; sleep 1000\r");
        assert!(wait_until(Duration::from_secs(5), || !s.is_idle()));
        let pid = s.child_pid.unwrap();
        let t = Instant::now();
        s.shutdown();
        assert!(t.elapsed() < Duration::from_millis(50), "shutdown blocked");
        // The shell is gone within the SIGHUP grace period + SIGKILL.
        assert!(wait_until(Duration::from_secs(5), || unsafe {
            libc::kill(pid as i32, 0) != 0
        }));
    }

    #[test]
    fn alt_screen_enter_exit_restores_main_screen() {
        let mut s = PtySession::spawn(
            sh(r"echo main-line; printf '\033[?1049h\033[2J\033[Halt-content'; sleep 0.3; printf '\033[?1049l'; echo back; sleep 0.3"),
            GridSize::DEFAULT,
        )
        .unwrap();
        assert!(wait_until(Duration::from_secs(5), || s
            .snapshot(100)
            .alt_screen));
        let alt = s.snapshot(100);
        assert!(alt.lines.iter().any(|l| l.contains("alt-content")));
        assert!(!alt.lines.iter().any(|l| l.contains("main-line")));
        assert!(alt.cursor.is_some());
        assert!(wait_until(Duration::from_secs(5), || s.poll_exited()));
        let main = s.snapshot(100);
        assert!(!main.alt_screen);
        assert!(main.lines.iter().any(|l| l.contains("main-line")));
        assert!(!main.lines.iter().any(|l| l.contains("alt-content")));
    }

    #[test]
    fn resize_reaches_the_program() {
        let mut s = PtySession::spawn(interactive_sh(), GridSize::DEFAULT).unwrap();
        s.resize(GridSize { cols: 77, rows: 19 });
        s.write(b"stty size\r");
        let ok = wait_until(Duration::from_secs(20), || {
            screen_text(&s).lines().any(|l| l.trim().ends_with("19 77"))
        });
        assert!(ok, "screen:\n{}", screen_text(&s));
    }

    #[test]
    fn long_line_without_newline_is_bounded() {
        // 4 MB with no newline: wraps in the grid instead of growing one
        // string forever.
        let mut s = PtySession::spawn(
            sh("head -c 4000000 /dev/zero | tr '\\0' a"),
            GridSize::DEFAULT,
        )
        .unwrap();
        assert!(wait_until(Duration::from_secs(60), || s.poll_exited()));
        let snap = s.snapshot(2_000);
        assert!(snap.lines.len() <= 2_000);
        assert!(snap.lines.iter().all(|l| l.chars().count() <= 100));
    }

    #[test]
    fn clear_keeps_cursor_line_and_drops_history() {
        let mut s = PtySession::spawn(
            sh("seq 1 300; printf 'prompt$ '; sleep 2"),
            GridSize::DEFAULT,
        )
        .unwrap();
        assert!(wait_until(Duration::from_secs(5), || screen_text(&s)
            .contains("prompt$")));
        s.clear();
        let snap = s.snapshot(2_000);
        assert_eq!(snap.lines, vec!["prompt$".to_string()]);
        s.shutdown();
        let _ = s.poll_exited();
    }

    #[test]
    fn paste_uses_cr_and_brackets_when_asked() {
        assert_eq!(paste_payload("a\nb\r\nc", false), b"a\rb\rc".to_vec());
        assert_eq!(
            paste_payload("x\x1b[201~y", true),
            b"\x1b[200~xy\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_reaches_the_program_bracketed_once_it_enables_2004() {
        let out = std::env::temp_dir().join(format!("malgel-bp-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        // The program asks for bracketed paste, then records raw input.
        let script = format!(
            "stty raw -echo; printf '\\033[?2004hready\\n'; head -c 15 > '{}'",
            out.display()
        );
        let mut s = PtySession::spawn(sh(&script), GridSize::DEFAULT).unwrap();
        assert!(wait_until(Duration::from_secs(5), || s
            .mode()
            .contains(TermMode::BRACKETED_PASTE)));
        s.paste("a\nb");
        assert!(wait_until(Duration::from_secs(5), || s.poll_exited()));
        let got = std::fs::read(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        assert_eq!(got, b"\x1b[200~a\rb\x1b[201~".to_vec());
    }

    #[test]
    fn rows_are_fetched_on_demand_and_clamped() {
        let mut s = PtySession::spawn(sh("seq 1 50; sleep 2"), GridSize::DEFAULT).unwrap();
        assert!(wait_until(Duration::from_secs(5), || s
            .view_info()
            .line_count
            >= 50));
        let info = s.view_info();
        let rows = s.rows(0..3);
        assert_eq!(rows, vec!["1", "2", "3"]);
        assert!(s.rows(info.line_count..info.line_count + 10).is_empty());
        assert_eq!(s.rows(49..50), vec!["50"]);
        s.shutdown();
        let _ = s.poll_exited();
    }

    #[test]
    fn colors_survive_as_style_runs() {
        let mut s = PtySession::spawn(
            sh(r"printf '\033[31mred\033[0m plain \033[1;44mbb\033[0m\n\033[7minv\033[0m\n'; sleep 1"),
            GridSize::DEFAULT,
        )
        .unwrap();
        assert!(wait_until(Duration::from_secs(5), || s
            .view_info()
            .line_count
            >= 2
            && screen_text(&s).contains("inv")));
        let info = s.view_info();
        let rows = s.styled_rows(0..info.line_count);
        let red = rows.iter().find(|r| r.text.starts_with("red")).unwrap();
        assert_eq!(red.text, "red plain bb");
        assert_eq!(red.runs[0].0, 0..3);
        assert_eq!(red.runs[0].1.fg, CellColor::Indexed(1));
        let bb = red
            .runs
            .iter()
            .find(|(r, _)| &red.text[r.clone()] == "bb")
            .unwrap();
        assert!(bb.1.bold);
        assert_eq!(bb.1.bg, CellColor::Indexed(4));
        let inv = rows.iter().find(|r| r.text == "inv").unwrap();
        assert!(inv.runs[0].1.is_inverse_default());
        s.shutdown();
        let _ = s.poll_exited();
    }

    #[test]
    fn encode_key_matches_xterm() {
        assert_eq!(
            encode_key("escape", None, false, false, false, false),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode_key("enter", None, false, false, false, false),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode_key("up", None, false, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key("up", None, false, false, false, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key("c", None, true, false, false, false),
            Some(vec![3])
        );
        assert_eq!(
            encode_key("d", None, true, false, false, false),
            Some(vec![4])
        );
        assert_eq!(
            encode_key("z", None, true, false, false, false),
            Some(vec![0x1a])
        );
        assert_eq!(
            encode_key("\\", None, true, false, false, false),
            Some(vec![0x1c])
        );
        assert_eq!(
            encode_key("q", Some("q"), false, false, false, false),
            Some(b"q".to_vec())
        );
        assert_eq!(
            encode_key(":", Some(":"), false, false, true, false),
            Some(b":".to_vec())
        );
        assert_eq!(
            encode_key("x", Some("x"), false, true, false, false),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            encode_key("pagedown", None, false, false, false, false),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(
            encode_key("right", None, true, false, false, false),
            Some(b"\x1b[1;5C".to_vec())
        );
    }
}

/// Selection on a headless emulator: no PTY, bytes fed straight to the
/// parser, so scrolling and history trimming are exact.
#[cfg(test)]
mod selection_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;

    fn term(cols: u16, rows: u16, scrollback: usize) -> Term<VoidListener> {
        Term::new(
            term_config(scrollback),
            &GridSize { cols, rows },
            VoidListener,
        )
    }

    fn feed(term: &mut Term<VoidListener>, text: &str) {
        let mut processor: ansi::Processor = ansi::Processor::new();
        processor.advance(term, text.replace('\n', "\r\n").as_bytes());
    }

    fn view_rows(term: &Term<VoidListener>) -> Vec<String> {
        let count = view_info_term(term).line_count;
        rows_term(term, 0..count)
    }

    fn row_of(term: &Term<VoidListener>, needle: &str) -> usize {
        view_rows(term)
            .iter()
            .position(|r| r == needle)
            .unwrap_or_else(|| panic!("{needle} not in view"))
    }

    #[test]
    fn drag_selects_cells_between_boundaries() {
        let mut t = term(40, 5, 100);
        feed(&mut t, "hello brave world\nsecond line\n");
        term_select_start(&mut t, SelectKind::Cells, 0, 6);
        term_select_update(&mut t, 0, 11);
        assert_eq!(term_selection_text(&t).as_deref(), Some("brave"));
        assert_eq!(term_selection_view(&t), Some(((0, 6), (0, 11))));
        // Across rows, backwards: anchor after "second", head at "brave".
        term_select_start(&mut t, SelectKind::Cells, 1, 6);
        term_select_update(&mut t, 0, 6);
        assert_eq!(
            term_selection_text(&t).as_deref(),
            Some("brave world\nsecond")
        );
        // A click without a drag selects nothing.
        term_select_start(&mut t, SelectKind::Cells, 0, 3);
        assert_eq!(term_selection_text(&t), None);
        assert_eq!(term_selection_view(&t), None);
    }

    #[test]
    fn double_click_selects_a_word_triple_click_a_line() {
        let mut t = term(40, 5, 100);
        feed(&mut t, "ls -la /tmp/some_dir file.txt\n");
        term_select_start(&mut t, SelectKind::Word, 0, 14);
        assert_eq!(term_selection_text(&t).as_deref(), Some("/tmp/some_dir"));
        // Dragging after a double-click grows by words.
        term_select_update(&mut t, 0, 24);
        assert_eq!(
            term_selection_text(&t).as_deref(),
            Some("/tmp/some_dir file.txt")
        );
        term_select_start(&mut t, SelectKind::Line, 0, 3);
        assert_eq!(
            term_selection_text(&t).as_deref(),
            Some("ls -la /tmp/some_dir file.txt")
        );
        assert_eq!(SelectKind::from_click_count(1), SelectKind::Cells);
        assert_eq!(SelectKind::from_click_count(2), SelectKind::Word);
        assert_eq!(SelectKind::from_click_count(3), SelectKind::Line);
    }

    #[test]
    fn wide_chars_map_view_columns_to_cells() {
        let mut t = term(40, 5, 100);
        feed(&mut t, "中文 ok\n");
        // View chars: 中 文 ' ' o k — the grid has spacer cells between.
        term_select_start(&mut t, SelectKind::Cells, 0, 1);
        term_select_update(&mut t, 0, 5);
        assert_eq!(term_selection_text(&t).as_deref(), Some("文 ok"));
        assert_eq!(term_selection_view(&t), Some(((0, 1), (0, 5))));
    }

    #[test]
    fn selection_follows_its_text_when_output_scrolls() {
        let mut t = term(20, 5, 1_000);
        let lines: String = (0..30).map(|i| format!("line {i}\n")).collect();
        feed(&mut t, &lines);
        let row = row_of(&t, "line 12");
        term_select_start(&mut t, SelectKind::Line, row, 0);
        assert_eq!(term_selection_text(&t).as_deref(), Some("line 12"));
        // More output pushes the screen into history; the selection stays
        // on "line 12", not on whatever is at its old row.
        let more: String = (30..60).map(|i| format!("line {i}\n")).collect();
        feed(&mut t, &more);
        assert_eq!(term_selection_text(&t).as_deref(), Some("line 12"));
        let ((r0, _), (r1, _)) = term_selection_view(&t).unwrap();
        assert_eq!(r0, row_of(&t, "line 12"));
        assert_eq!(r1, r0);
    }

    #[test]
    fn selection_survives_history_trimming_and_drops_with_its_lines() {
        // 100 lines of history: older output is dropped first.
        let mut t = term(20, 5, 100);
        let lines: String = (0..200).map(|i| format!("row {i}\n")).collect();
        feed(&mut t, &lines);
        let count = view_info_term(&t).line_count;
        assert!(count <= 100 + 5, "view {count} rows");
        assert!(!view_rows(&t).iter().any(|r| r == "row 50"));
        let row = row_of(&t, "row 150");
        term_select_start(&mut t, SelectKind::Line, row, 0);
        // 30 more lines trim 30 from the top: the selected row's index
        // moves up by 30, its text does not change.
        let more: String = (200..230).map(|i| format!("row {i}\n")).collect();
        feed(&mut t, &more);
        assert_eq!(term_selection_text(&t).as_deref(), Some("row 150"));
        let ((r0, _), _) = term_selection_view(&t).unwrap();
        assert_eq!(r0, row_of(&t, "row 150"));
        assert_eq!(r0 + 30, row);
        // Once its line is trimmed away the selection is gone, not moved
        // onto other text.
        let flood: String = (230..400).map(|i| format!("row {i}\n")).collect();
        feed(&mut t, &flood);
        assert_eq!(term_selection_text(&t), None);
        assert_eq!(term_selection_view(&t), None);
    }

    #[test]
    fn partly_trimmed_selection_keeps_the_part_still_kept() {
        let mut t = term(20, 5, 100);
        let lines: String = (0..150).map(|i| format!("r{i}\n")).collect();
        feed(&mut t, &lines);
        let a = row_of(&t, "r60");
        let b = row_of(&t, "r70");
        term_select_start(&mut t, SelectKind::Line, a, 0);
        term_select_update(&mut t, b, 0);
        // Trim up to r64: r60..r64 leave, the rest stays selected.
        let top: usize = view_rows(&t)[0][1..].parse().unwrap();
        let more: String = (150..150 + (65 - top)).map(|i| format!("r{i}\n")).collect();
        feed(&mut t, &more);
        let text = term_selection_text(&t).unwrap();
        assert!(text.starts_with("r65"), "{text}");
        assert!(text.ends_with("r70"), "{text}");
        assert_eq!(term_selection_view(&t).unwrap().0, (0, 0));
    }

    #[test]
    fn scrollback_setting_is_clamped_and_shrinks_live() {
        assert_eq!(clamp_scrollback(0), MIN_SCROLLBACK_LINES);
        assert_eq!(clamp_scrollback(10_000), 10_000);
        assert_eq!(clamp_scrollback(usize::MAX), MAX_SCROLLBACK_LINES);
        let mut t = term(20, 5, 1_000);
        let lines: String = (0..800).map(|i| format!("{i}\n")).collect();
        feed(&mut t, &lines);
        assert!(view_info_term(&t).line_count > 700);
        t.set_options(term_config(200));
        let rows = view_rows(&t);
        assert!(rows.len() <= 200 + 5, "{} rows", rows.len());
        // The newest lines are the ones kept.
        assert_eq!(rows.iter().rev().find(|r| !r.is_empty()).unwrap(), "799");
    }

    #[test]
    fn select_all_covers_the_view() {
        let mut t = term(20, 5, 100);
        feed(&mut t, "a\nb\nc");
        term_select_all(&mut t);
        assert_eq!(term_selection_text(&t).as_deref(), Some("a\nb\nc"));
    }

    #[test]
    fn bracketed_paste_follows_mode_2004() {
        let mut t = term(20, 5, 100);
        assert!(!t.mode().contains(TermMode::BRACKETED_PASTE));
        feed(&mut t, "\x1b[?2004h");
        assert!(t.mode().contains(TermMode::BRACKETED_PASTE));
        assert_eq!(
            paste_payload("echo 1\necho 2\n", true),
            b"\x1b[200~echo 1\recho 2\r\x1b[201~".to_vec()
        );
        feed(&mut t, "\x1b[?2004l");
        assert!(!t.mode().contains(TermMode::BRACKETED_PASTE));
    }
}
