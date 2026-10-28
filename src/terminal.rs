//! Integrated terminal: a bottom panel with a real shell (Ctrl+`).
//!
//! Open/focus/refresh follow VS Code habits: summoning shows the panel and
//! focuses it when focus is elsewhere, and only hides when the terminal
//! already owns focus. Output follows the tail; scrolling up pauses follow.
//! New Terminal is Ctrl+Shift+` (never the same chord as toggle).
//!
//! The screen is a real VT emulator ([`crate::term_session`]): vim, less, top
//! and git's pager draw correctly, the alternate screen restores the shell
//! transcript on exit, and the PTY is sized to the visible cells. Parsing
//! happens on the session's reader thread; the panel only paints the rows in
//! view (a virtual list), so a flood costs the UI a few dozen rows a frame.
//!
//! Input is direct, like xterm / VS Code: every key goes to the program as it
//! is pressed, so Ctrl+C / D / Z / \, Tab completion, Up/Down history, Ctrl+R,
//! Ctrl+L, Ctrl+U / W and full-screen apps all behave as in a terminal. A
//! keystroke interceptor routes non-text keys before the text input's own
//! bindings (Copy on Ctrl+C, Undo on Ctrl+Z) can claim them. Typed text still
//! lands in a one-line input first — the IME's composition shows there — and
//! is forwarded the moment it is committed. Before a shell runs, that input is
//! a plain command line ("Run a command…").
//!
//! App chords keep working from the terminal: Ctrl+Shift+anything (palette,
//! Ctrl+Shift+C / V copy and paste), Ctrl+P, Ctrl+`, Ctrl+Tab, Ctrl+PageUp /
//! PageDown, Ctrl+Q. Element ids are unique per session so split panes do not
//! share hit-testing, and keys only ever reach the focused pane. Kill closes
//! the session tab when more than one exists; the last session shows a
//! Restart affordance instead of a zombie empty tab.

use std::{
    cell::Cell,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, ElementId, Entity, EntityInputHandler as _,
    EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight, Global, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Pixels, Point, Render, ScrollDelta, ScrollStrategy, ScrollWheelEvent,
    SharedString, Size, Styled as _, StyledText, UnderlineStyle, UniformListScrollHandle,
    WeakEntity, Window,
    assets::IconName,
    base::Disableable as _,
    canvas,
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt as _, PopupMenuItem},
        v_flex,
    },
    div,
    prelude::FluentBuilder as _,
    px, rems, uniform_list,
};
use portable_pty::CommandBuilder;

use crate::term_session::{
    CellColor, CellStyle, GridSize, PtySession, SelectKind, StyledRow, ViewInfo, encode_key,
};

/// Transcript kept from a stopped / exited shell above the next one.
const MAX_PRELUDE_LINES: usize = 500;
const LINE_HEIGHT_REMS: f32 = 1.35;
/// `text_sm` in rems: the output font size.
const FONT_REMS: f32 = 0.875;
/// UI poll for new PTY output. Parsing happens on the reader thread; a tick
/// only reads the view's shape when something changed.
const PUMP_INTERVAL: Duration = Duration::from_millis(16);
/// While output streams (a flood, not an echo), repaint at most this often:
/// nobody reads 60 frames of `yes`, and every frame repaints the window.
const FLOOD_FRAME: Duration = Duration::from_millis(100);
/// A flood that has gone on this long (`yes`, a runaway log) drops to
/// [`SUSTAINED_FLOOD_FRAME`]: on a software renderer each frame of a full
/// window costs a core-fraction, and the text is a blur anyway.
const SUSTAINED_FLOOD_AFTER: Duration = Duration::from_millis(500);
const SUSTAINED_FLOOD_FRAME: Duration = Duration::from_millis(333);
/// How long after Ctrl+C/Z/\\ output is painted unthrottled.
const INTERRUPT_FAST_PAINT: Duration = Duration::from_millis(1000);
/// Output quiet this long after an interrupt counts as settled.
const SETTLE_QUIET: Duration = Duration::from_millis(500);
/// Bytes per tick below which output counts as interactive (echo, prompt)
/// and is painted at once.
const ECHO_BYTES: u64 = 4096;
/// Shortest gap between two paints of small output (echoes): key repeat or
/// fast typing then costs ~30 frames a second, not one per character.
const ECHO_FRAME: Duration = Duration::from_millis(33);

/// Every live panel, for the keystroke interceptor.
#[derive(Default)]
struct TerminalRegistry(Vec<WeakEntity<TerminalPanel>>);

impl Global for TerminalRegistry {}

/// Install the keystroke interceptor that gives the focused terminal first
/// claim on keys. Call once at startup, after the key bindings.
pub fn init(cx: &mut App) {
    cx.set_global(TerminalRegistry::default());
    cx.intercept_keystrokes(|event, window, cx| {
        let panels: Vec<Entity<TerminalPanel>> = cx
            .global::<TerminalRegistry>()
            .0
            .iter()
            .filter_map(|weak| weak.upgrade())
            .collect();
        for panel in panels {
            if !panel.read(cx).contains_focus(window, cx) {
                continue;
            }
            let handled = panel.update(cx, |panel, cx| {
                panel.intercept_key(&event.keystroke, window, cx)
            });
            if handled {
                cx.stop_propagation();
            }
            break;
        }
    })
    .detach();
}

/// App shortcuts that must keep working while the terminal has focus.
fn is_app_chord(keys: &Keystroke) -> bool {
    let m = &keys.modifiers;
    if m.platform || m.function {
        return true;
    }
    if m.control && m.shift {
        // Ctrl+Shift+C / V are the terminal's own copy / paste.
        return !matches!(keys.key.as_str(), "c" | "v");
    }
    m.control
        && !m.alt
        && matches!(
            keys.key.as_str(),
            "`" | "p" | "q" | "tab" | "pageup" | "pagedown" | ","
        )
}

/// One integrated terminal session in the bottom panel.
pub struct TerminalPanel {
    /// The panel is maximized (set by the workspace; picks the header icon).
    pub maximized: bool,
    focus_handle: FocusHandle,
    cwd: Option<PathBuf>,
    input: Entity<InputState>,
    scroll: UniformListScrollHandle,
    session: Option<PtySession>,
    /// Transcript of earlier (stopped) shells plus status notices.
    prelude: Vec<SharedString>,
    /// Shape of the session's view as of the last tick.
    view: ViewInfo,
    /// Output viewport size from the last paint, for PTY resize.
    viewport: Rc<Cell<Option<Size<Pixels>>>>,
    /// Character cell (width, height) from the last render.
    cell: (Pixels, Pixels),
    rem: Pixels,
    last_paint: Instant,
    bytes_at_paint: u64,
    /// When the current flood (painting throttled) began.
    flood_since: Option<Instant>,
    /// After an interrupt, paint every tick until then.
    fast_paint_until: Option<Instant>,
    /// Last interrupt and the last paint after it that showed new output:
    /// MALGEL_PERF logs `term_interrupt_settle` (interrupt to the final
    /// changed frame) once output has been quiet for SETTLE_QUIET.
    interrupt_at: Option<Instant>,
    settle_paint: Option<Instant>,
    _pump: Option<gpui_kit::Task<()>>,
    /// Stick output viewport to the end while the user has not scrolled up.
    follow_tail: bool,
    /// Selection in the transcript of earlier shells (the prelude, which
    /// never scrolls under a running shell): anchor, head and how it grows.
    /// A selection in the live session is kept by the emulator instead
    /// ([`PtySession::select_start`]), anchored to buffer lines.
    selection: Option<(Cell2, Cell2, SelectKind)>,
    /// The left button went down in the output and has not come up yet,
    /// and where that selection lives.
    selecting: Option<SelTarget>,
}

/// Which buffer a drag selects in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelTarget {
    Prelude,
    Session,
}

/// A character cell position in view rows: (row, column).
type Cell2 = (usize, usize);

/// Events the workspace listens for (Kill → close session tab).
pub enum TerminalEvent {
    /// User asked to kill this session (toolbar Kill). Workspace closes the tab
    /// when another session remains; otherwise leaves a Restart affordance.
    CloseSession,
    /// Toolbar Close (X): hide the panel, sessions keep running (VS Code).
    HidePanel,
    /// The shell exited on its own (`exit`, Ctrl+D): VS Code closes the
    /// terminal, and hides the panel when it was the last one.
    Exited,
}

impl EventEmitter<TerminalEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl TerminalPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(""));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let line = input.read(cx).value().to_string();
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    // No shell yet (or an IME line pending): start one and run it.
                    this.send_line(&line, window, cx);
                }
                InputEvent::Change => this.forward_typed(window, cx),
                _ => {}
            },
        )
        .detach();
        let focus_handle = cx.focus_handle();
        // A click anywhere in the panel focuses its root (track_focus) after
        // our own mouse handlers ran; hand that focus straight to the input so
        // typed text (which needs the input's text handler) is never dropped.
        cx.on_focus(&focus_handle, window, |this, window, cx| {
            this.focus_input(window, cx);
        })
        .detach();
        if cx.has_global::<TerminalRegistry>() {
            let weak = cx.weak_entity();
            let registry = cx.global_mut::<TerminalRegistry>();
            registry.0.retain(|w| w.upgrade().is_some());
            registry.0.push(weak);
        }
        Self {
            focus_handle,
            cwd: None,
            input,
            scroll: UniformListScrollHandle::new(),
            session: None,
            prelude: Vec::new(),
            view: ViewInfo::default(),
            viewport: Rc::new(Cell::new(None)),
            cell: (px(8.), px(18.)),
            rem: px(16.),
            last_paint: Instant::now(),
            bytes_at_paint: 0,
            flood_since: None,
            fast_paint_until: None,
            maximized: false,
            interrupt_at: None,
            settle_paint: None,
            _pump: None,
            follow_tail: true,
            selection: None,
            selecting: None,
        }
    }

    /// Folder new shells start in (shown in the tab's tooltip).
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    fn shell_running(&self) -> bool {
        self.session.is_some()
    }

    /// Change the folder new shells start in. A running shell is restarted
    /// only when it is idle at its prompt: summoning the panel from a file in
    /// another folder must never kill a build or server the user started.
    pub fn set_cwd(&mut self, cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        if self.session.as_ref().is_some_and(|s| s.is_idle()) {
            self.start_shell(window, cx);
        }
        cx.notify();
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        // Focus the Input key-sink so the window has an input_handler.
        // X11 IME commits (and `xdotool type`) are dropped without one; raw
        // KeyDown alone is not enough for typed characters under Xvfb.
        let running = self.shell_running();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(if running { "" } else { "Run a command…" }, window, cx);
        });
        self.input.focus_handle(cx).focus(window, cx);
    }

    /// True when this session's panel or composer owns keyboard focus.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
            || self.input.focus_handle(cx).contains_focused(window, cx)
    }

    /// Restart a killed shell and focus the composer (last-session affordance).
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_shell(window, cx);
        self.follow_tail = true;
        self.scroll_to_end();
        self.focus_input(window, cx);
        cx.notify();
    }

    fn total_rows(&self) -> usize {
        if self.view.alt_screen {
            self.view.line_count
        } else {
            self.prelude.len() + self.view.line_count
        }
    }

    /// Keep the viewport on the newest output (VS Code follow-tail).
    fn scroll_to_end(&self) {
        let rows = self.total_rows();
        if rows > 0 {
            self.scroll.scroll_to_item(rows - 1, ScrollStrategy::Bottom);
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.prelude.clear();
        self.selection = None;
        if let Some(session) = &self.session {
            session.clear_selection();
        }
        if let Some(session) = &self.session {
            session.clear();
            self.view = session.view_info();
        } else {
            self.view = ViewInfo::default();
        }
        self.follow_tail = true;
        self.scroll_to_end();
        cx.notify();
    }

    /// Rows above the live session's first row in view coordinates.
    fn session_offset(&self) -> usize {
        if self.view.alt_screen {
            0
        } else {
            self.prelude.len()
        }
    }

    /// Ordered selection bounds in view rows (end column exclusive), `None`
    /// when nothing (or an empty span) is selected.
    fn selection_span(&self) -> Option<(Cell2, Cell2)> {
        if let Some((a, b, kind)) = self.selection {
            let texts = |row: usize| {
                self.row_texts(row..row + 1)
                    .pop()
                    .map(|r| r.text)
                    .unwrap_or_default()
            };
            let (start, end) = expand_selection(a, b, kind, texts);
            return (start != end).then_some((start, end));
        }
        let ((r0, c0), (r1, c1)) = self.session.as_ref()?.selection_view()?;
        let off = self.session_offset();
        Some(((r0 + off, c0), (r1 + off, c1)))
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        if let Some(session) = &self.session {
            session.clear_selection();
        }
    }

    /// The selected text (VS Code): rows between the ends, trailing blanks
    /// trimmed per row; in the live session soft-wrapped rows join up.
    fn selected_text(&self) -> Option<String> {
        if self.selection.is_none() {
            return self.session.as_ref()?.selection_text();
        }
        let ((r0, c0), (r1, c1)) = self.selection_span()?;
        let rows = self.row_texts(r0..r1 + 1);
        let mut out = String::new();
        for (i, row) in rows.iter().enumerate() {
            let ix = r0 + i;
            let from = if ix == r0 { c0 } else { 0 };
            let to = if ix == r1 { c1 } else { usize::MAX };
            let piece: String = row
                .text
                .chars()
                .skip(from)
                .take(to.saturating_sub(from))
                .collect();
            if i > 0 {
                out.push('\n');
            }
            out.push_str(piece.trim_end());
        }
        (!out.is_empty()).then_some(out)
    }

    /// Copy the output selection; with none, the clipboard is left alone
    /// (VS Code's Copy does nothing without a selection).
    fn copy_selection(&self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Select everything in view (the context menu's Select All).
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let rows = self.total_rows();
        if self.session_offset() == 0
            && let Some(session) = &self.session
        {
            self.selection = None;
            session.select_all();
        } else if rows > 0 {
            if let Some(session) = &self.session {
                session.clear_selection();
            }
            self.selection = Some(((0, 0), (rows - 1, usize::MAX), SelectKind::Cells));
        }
        cx.notify();
    }

    /// Mouse down in the output: start a selection of `kind` (click count)
    /// at `pos`, or with Shift extend the current one.
    fn begin_selection(&mut self, pos: Point<Pixels>, kind: SelectKind, extend: bool) {
        let Some((row, col)) = self.cell_at(pos, kind) else {
            self.clear_selection();
            self.selecting = None;
            return;
        };
        let off = self.session_offset();
        if extend && let Some(target) = self.current_target() {
            self.selecting = Some(target);
            self.update_selection_to(row, col);
            return;
        }
        if row >= off
            && let Some(session) = &self.session
        {
            self.selection = None;
            session.select_start(kind, row - off, col);
            self.selecting = Some(SelTarget::Session);
        } else {
            if let Some(session) = &self.session {
                session.clear_selection();
            }
            self.selection = Some(((row, col), (row, col), kind));
            self.selecting = Some(SelTarget::Prelude);
        }
    }

    fn current_target(&self) -> Option<SelTarget> {
        if self.selection.is_some() {
            Some(SelTarget::Prelude)
        } else if self
            .session
            .as_ref()
            .is_some_and(|s| s.selection_view().is_some())
        {
            Some(SelTarget::Session)
        } else {
            None
        }
    }

    /// Drag: move the selection's head to the cell under `pos`.
    fn drag_selection(&mut self, pos: Point<Pixels>) {
        let kind = match self.selection {
            Some((_, _, kind)) => kind,
            None => SelectKind::Cells,
        };
        if let Some((row, col)) = self.cell_at(pos, kind) {
            self.update_selection_to(row, col);
        }
    }

    fn update_selection_to(&mut self, row: usize, col: usize) {
        let off = self.session_offset();
        match self.selecting {
            Some(SelTarget::Prelude) => {
                if let Some((anchor, _, kind)) = self.selection {
                    self.selection = Some((anchor, (row, col), kind));
                }
            }
            Some(SelTarget::Session) => {
                if let Some(session) = &self.session {
                    // The live session cannot select into the transcript
                    // above it: clamp to its first cell.
                    let (r, c) = if row < off { (0, 0) } else { (row - off, col) };
                    session.select_update(r, c);
                }
            }
            None => {}
        }
    }

    /// Output cell under window position `pos`. For a drag the column is
    /// rounded to the nearest boundary, so it selects the characters it
    /// passes; a double / triple click takes the character under it.
    fn cell_at(&self, pos: Point<Pixels>, kind: SelectKind) -> Option<Cell2> {
        let rows = self.total_rows();
        if rows == 0 {
            return None;
        }
        let state = self.scroll.0.borrow();
        let bounds = state.base_handle.bounds();
        let offset = state.base_handle.offset();
        let (cell_w, line_h) = (f32::from(self.cell.0), f32::from(self.cell.1));
        if cell_w <= 0. || line_h <= 0. {
            return None;
        }
        let y = f32::from(pos.y - bounds.origin.y - offset.y);
        let x = f32::from(pos.x - bounds.origin.x - offset.x);
        let row = ((y / line_h).floor().max(0.) as usize).min(rows - 1);
        let cells = (x / cell_w).max(0.);
        let col = if kind == SelectKind::Cells {
            cells.round()
        } else {
            cells.floor()
        } as usize;
        Some((row, col))
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        if let Some(session) = &self.session {
            session.paste(&text);
            self.follow_tail = true;
        }
    }

    pub fn ensure_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            self.start_shell(window, cx);
        }
        // Re-arm follow-tail so opening via shortcut always shows latest output.
        self.follow_tail = true;
        self.scroll_to_end();
        self.focus_input(window, cx);
        cx.notify();
    }

    /// Stop the shell. Never blocks: the kill and reap run off the UI thread.
    pub fn stop_shell(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.shutdown();
            // Keep what was on screen (normal screen only — a TUI's frame is
            // noise once it is gone).
            let snap = session.snapshot(MAX_PRELUDE_LINES);
            if !snap.alt_screen {
                self.prelude
                    .extend(snap.lines.into_iter().map(SharedString::from));
            }
        }
        self._pump = None;
        self.view = ViewInfo::default();
        self.selection = None;
        self.selecting = None;
        cap_prelude(&mut self.prelude);
    }

    fn send_line(&mut self, line: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_none() {
            self.start_shell(window, cx);
        }
        let Some(session) = &self.session else {
            self.append("Couldn’t start a shell.\n", cx);
            return;
        };
        // PTY line discipline expects CR (same as a real keyboard Enter).
        let mut payload = line.as_bytes().to_vec();
        payload.push(b'\r');
        session.write(&payload);
        self.follow_tail = true;
        self.scroll_to_end();
    }

    /// Committed text in the input goes straight to the program (direct
    /// input). An IME composition stays in the input until committed.
    fn forward_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let (text, composing) = self.input.update(cx, |input, cx| {
            (
                input.value().to_string(),
                input.marked_text_range(window, cx).is_some(),
            )
        });
        if text.is_empty() || composing {
            return;
        }
        session.write(text.as_bytes());
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.follow_tail = true;
    }

    /// First claim on a keystroke while this panel has focus (runs before
    /// any binding). Returns true when the key went to the program.
    fn intercept_key(
        &mut self,
        keys: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.session.is_none() || is_app_chord(keys) {
            return false;
        }
        let m = &keys.modifiers;
        if m.control && m.shift {
            match keys.key.as_str() {
                "c" => self.copy_selection(cx),
                _ => self.paste_clipboard(cx),
            }
            return true;
        }
        let (pending, has_selection) = {
            let input = self.input.read(cx);
            (
                !input.value().is_empty(),
                !input.selected_value().is_empty(),
            )
        };
        let ctrl_only = m.control && !m.alt && !m.shift;
        if ctrl_only && keys.key == "c" && has_selection {
            return false; // copy the input's selection
        }
        if ctrl_only && keys.key == "v" {
            self.paste_clipboard(cx);
            return true;
        }
        let session = self.session.as_ref().expect("checked above");
        if ctrl_only && matches!(keys.key.as_str(), "c" | "z" | "\\") {
            let byte = match keys.key.as_str() {
                "c" => 0x03,
                "z" => 0x1a,
                _ => 0x1c,
            };
            // Signals jump any queued input; a half-typed IME line is dropped.
            session.interrupt(byte);
            // Skip the flood frame rate: paint what is there now and keep
            // painting every tick until the prompt is up.
            self.flood_since = None;
            self.fast_paint_until = Some(Instant::now() + INTERRUPT_FAST_PAINT);
            self.interrupt_at = Some(Instant::now());
            self.settle_paint = None;
            if pending {
                self.input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            self.follow_tail = true;
            self.scroll_to_end();
            cx.notify();
            return true;
        }
        // An IME composition (or text typed before the shell started) owns
        // editing keys until it is committed.
        if pending && !m.control && !m.alt {
            return false;
        }
        // Plain text goes through the input so the IME and X11 commits work;
        // it is forwarded on change.
        let printable =
            !m.control && !m.alt && (keys.key == "space" || keys.key.chars().count() == 1);
        if printable {
            let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
            // On Linux an IME consumes the keys it composes before they get
            // here, so a plain key with its text goes straight to the shell:
            // no composer edit, change event and clear per keystroke — three
            // UI updates (and frames) fewer for every key typed.
            let direct = cfg!(target_os = "linux")
                && !pending
                && keys
                    .key_char
                    .as_deref()
                    .is_some_and(|text| !text.is_empty());
            if input_focused && direct {
                session.write(keys.key_char.as_deref().unwrap_or_default().as_bytes());
                self.follow_tail = true;
                return true;
            }
            if input_focused {
                return false;
            }
            // Focus is on the panel itself (a click on the output can land
            // the panel's own focus after the input's, and the redirect runs
            // on a later frame): the text has no input to go to, so send it
            // straight to the shell and put focus back for the next key.
            let text = match keys.key_char.as_deref() {
                Some(text) if !text.is_empty() => text.to_string(),
                _ if keys.key == "space" => " ".to_string(),
                _ if m.shift => keys.key.to_uppercase(),
                _ => keys.key.clone(),
            };
            session.write(text.as_bytes());
            self.follow_tail = true;
            self.scroll_to_end();
            self.focus_input(window, cx);
            return true;
        }
        let Some(bytes) = encode_key(
            &keys.key,
            keys.key_char.as_deref(),
            m.control,
            m.alt,
            m.shift,
            self.view.app_cursor,
        ) else {
            return false;
        };
        session.write(&bytes);
        self.follow_tail = true;
        self.scroll_to_end();
        true
    }

    /// Show a status line (banner, "shell stopped") under the transcript.
    pub fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.inject(text);
            self.view = session.view_info();
        } else {
            for line in text.trim_end_matches('\n').split('\n') {
                self.prelude.push(SharedString::from(line.to_string()));
            }
            let dropped = cap_prelude(&mut self.prelude);
            if dropped > 0 {
                self.selection = self.selection.and_then(|sel| shift_selection(sel, dropped));
            }
        }
        if self.follow_tail {
            self.scroll_to_end();
        }
        cx.notify();
    }

    fn start_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_shell();
        let size = self.wanted_grid_size().unwrap_or(GridSize::DEFAULT);
        let mut cmd = default_shell(crate::themes::AppSettings::get(cx).terminal_shell_config);
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        match PtySession::spawn(cmd, size) {
            Ok(session) => self.session = Some(session),
            Err(err) => {
                self.append(&format!("{err}\n"), cx);
                return;
            }
        }
        self.bytes_at_paint = 0;
        self.flood_since = None;
        self._pump = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(PUMP_INTERVAL).await;
                let cont = this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .unwrap_or(false);
                if !cont {
                    break;
                }
            }
        }));
    }

    /// Grid that fits the output viewport painted last frame.
    fn wanted_grid_size(&self) -> Option<GridSize> {
        let size = self.viewport.get()?;
        Some(grid_size_for(
            size.width,
            size.height,
            self.cell.0,
            self.cell.1,
            self.rem,
        ))
    }

    /// One UI frame of terminal work. Returns whether to keep pumping.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let wanted = self.wanted_grid_size();
        let Some(session) = &mut self.session else {
            return false;
        };
        if let Some(size) = wanted {
            session.resize(size);
        }
        let exited = session.poll_exited();
        if !exited {
            let bytes = session.bytes_read();
            let streaming = bytes.saturating_sub(self.bytes_at_paint) > ECHO_BYTES;
            let frame = match self.flood_since {
                Some(since) if since.elapsed() >= SUSTAINED_FLOOD_AFTER => SUSTAINED_FLOOD_FRAME,
                _ => FLOOD_FRAME,
            };
            let fast = self
                .fast_paint_until
                .is_some_and(|until| Instant::now() < until);
            if !streaming && !fast && self.last_paint.elapsed() < ECHO_FRAME {
                // Fast typing: one paint per ECHO_FRAME for all the echoes.
                return true;
            }
            if streaming && !fast && self.last_paint.elapsed() < frame {
                // Leave the dirty flag set; paint on a later tick.
                return true;
            }
            if !session.take_dirty() {
                if self.last_paint.elapsed() > FLOOD_FRAME {
                    self.flood_since = None;
                }
                if let Some(at) = self.interrupt_at {
                    let last = self.settle_paint.unwrap_or(at);
                    if last.elapsed() >= SETTLE_QUIET {
                        self.interrupt_at = None;
                        crate::perf::mark(&format!(
                            "term_interrupt_settle {:.0}ms",
                            last.duration_since(at).as_secs_f64() * 1000.0
                        ));
                    }
                }
                return true;
            }
            if self.interrupt_at.is_some() {
                self.settle_paint = Some(Instant::now());
            }
            if streaming && !fast {
                self.flood_since.get_or_insert_with(Instant::now);
            } else {
                self.flood_since = None;
            }
            self.bytes_at_paint = bytes;
        }
        self.last_paint = Instant::now();
        self.view = session.view_info();
        if exited {
            self.stop_shell();
            self.append("— shell exited\n", cx);
            let _ = window;
            cx.emit(TerminalEvent::Exited);
        }
        if self.follow_tail {
            self.scroll_to_end();
        }
        crate::perf::note_term_paint();
        cx.notify();
        !exited
    }

    /// Styled view rows `range` (prelude, then the live session).
    fn row_texts(&self, range: Range<usize>) -> Vec<StyledRow> {
        if self.view.alt_screen {
            let Some(session) = &self.session else {
                return Vec::new();
            };
            return session.styled_rows(range);
        }
        let split = self.prelude.len();
        let mut out = Vec::with_capacity(range.len());
        if range.start < split {
            out.extend(
                self.prelude[range.start..range.end.min(split)]
                    .iter()
                    .map(|line| StyledRow {
                        text: line.to_string(),
                        runs: Vec::new(),
                    }),
            );
        }
        if range.end > split
            && let Some(session) = &self.session
        {
            let from = range.start.saturating_sub(split);
            let to = range.end - split;
            out.extend(session.styled_rows(from..to));
        }
        out
    }

    /// Cursor row in view coordinates.
    fn cursor_row(&self) -> Option<(usize, usize)> {
        let (row, col) = self.view.cursor?;
        let offset = if self.view.alt_screen {
            0
        } else {
            self.prelude.len()
        };
        Some((row + offset, col))
    }
}

/// Columns × rows of `cell_w` × `line_h` cells that fit a viewport with the
/// output area's 0.5rem padding on each side.
fn grid_size_for(
    width: Pixels,
    height: Pixels,
    cell_w: Pixels,
    line_h: Pixels,
    rem: Pixels,
) -> GridSize {
    let w = f32::from(width) - f32::from(rem);
    let h = f32::from(height) - f32::from(rem);
    let cols = (w / f32::from(cell_w).max(1.)).floor().max(0.) as usize;
    let rows = (h / f32::from(line_h).max(1.)).floor().max(0.) as usize;
    GridSize::clamped(cols, rows)
}

/// Theme-following terminal colors: the 16 ANSI colors use VS Code's
/// terminal palette for the light or dark theme; 16–255 are xterm's cube
/// and gray ramp.
#[derive(Clone, Copy)]
struct TermColors {
    fg: Hsla,
    bg: Hsla,
    ansi: [u32; 16],
}

const VSCODE_DARK_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xe5e5e5,
];
const VSCODE_LIGHT_ANSI: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, 0x666666,
    0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

impl TermColors {
    fn new(dark: bool, fg: Hsla, bg: Hsla) -> Self {
        Self {
            fg,
            bg,
            ansi: if dark {
                VSCODE_DARK_ANSI
            } else {
                VSCODE_LIGHT_ANSI
            },
        }
    }

    fn resolve(&self, color: CellColor, default: Hsla) -> Hsla {
        match color {
            CellColor::Default => default,
            CellColor::Rgb(r, g, b) => rgb_hsla(r, g, b),
            CellColor::Indexed(ix) => {
                let (r, g, b) = xterm_rgb(ix, &self.ansi);
                rgb_hsla(r, g, b)
            }
        }
    }

    fn highlight(&self, style: &CellStyle) -> HighlightStyle {
        let (fg, bg) = if style.is_inverse_default() {
            (self.bg, Some(self.fg))
        } else {
            let bg = (style.bg != CellColor::Default).then(|| self.resolve(style.bg, self.bg));
            (self.resolve(style.fg, self.fg), bg)
        };
        HighlightStyle {
            color: Some(if style.dim { fg.opacity(0.6) } else { fg }),
            background_color: bg,
            font_weight: style.bold.then_some(FontWeight::BOLD),
            font_style: style.italic.then_some(FontStyle::Italic),
            underline: style.underline.then(|| UnderlineStyle {
                thickness: px(1.),
                color: Some(fg),
                wavy: false,
            }),
            ..HighlightStyle::default()
        }
    }
}

fn rgb_hsla(r: u8, g: u8, b: u8) -> Hsla {
    gpui_kit::rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
}

/// xterm-256 palette entry `ix`, with 0–15 from `ansi`.
fn xterm_rgb(ix: u8, ansi: &[u32; 16]) -> (u8, u8, u8) {
    match ix {
        0..=15 => {
            let c = ansi[ix as usize];
            ((c >> 16) as u8, (c >> 8) as u8, c as u8)
        }
        16..=231 => {
            let n = ix - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (ix - 232) * 10;
            (v, v, v)
        }
    }
}

/// Text and highlight runs for one row, with the cursor (char column) drawn
/// as an inverted cell. Runs come back sorted and non-overlapping.
fn row_highlights(
    row: StyledRow,
    cursor_col: Option<usize>,
    selected: Option<(Range<usize>, Hsla)>,
    colors: &TermColors,
) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
    let StyledRow { mut text, runs } = row;
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = runs
        .iter()
        .map(|(range, style)| (range.clone(), colors.highlight(style)))
        .collect();
    if let Some((cols, bg)) = selected {
        let byte = |col: usize| text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
        let (start, end) = (byte(cols.start), byte(cols.end));
        if start < end {
            highlights = overlay_background(highlights, start..end, bg);
        }
    }
    if let Some(col) = cursor_col {
        // Pad so the cursor sits at its column.
        let chars = text.chars().count();
        if chars <= col {
            text.extend(std::iter::repeat_n(' ', col + 1 - chars));
        }
        let start = text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
        let end = text[start..]
            .chars()
            .next()
            .map_or(text.len(), |c| start + c.len_utf8());
        let under = runs
            .iter()
            .find(|(r, _)| r.start <= start && start < r.end)
            .map(|(_, s)| colors.highlight(s));
        let cell_fg = under.and_then(|h| h.color).unwrap_or(colors.fg);
        let cell_bg = under.and_then(|h| h.background_color).unwrap_or(colors.bg);
        let cursor = HighlightStyle {
            color: Some(cell_bg),
            background_color: Some(cell_fg),
            ..under.unwrap_or_default()
        };
        let mut split = Vec::with_capacity(highlights.len() + 2);
        for (range, style) in highlights {
            if range.end <= start || range.start >= end {
                split.push((range, style));
                continue;
            }
            if range.start < start {
                split.push((range.start..start, style));
            }
            if range.end > end {
                split.push((end..range.end, style));
            }
        }
        split.push((start..end, cursor));
        split.sort_by_key(|(r, _)| r.start);
        highlights = split;
    }
    if text.is_empty() {
        // Non-breaking space keeps an empty row's height.
        text.push('\u{00a0}');
    }
    (SharedString::from(text), highlights)
}

/// `highlights` (sorted, disjoint) with `bg` painted under `span`: runs
/// inside it keep their colors, gaps get the background alone.
fn overlay_background(
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    span: Range<usize>,
    bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let plain = HighlightStyle {
        background_color: Some(bg),
        ..Default::default()
    };
    let mut out = Vec::with_capacity(highlights.len() + 3);
    let mut at = span.start;
    for (range, style) in highlights {
        if range.end <= span.start || range.start >= span.end {
            out.push((range, style));
            continue;
        }
        if range.start < span.start {
            out.push((range.start..span.start, style));
        }
        let from = range.start.max(span.start);
        let to = range.end.min(span.end);
        if from > at {
            out.push((at..from, plain));
        }
        out.push((
            from..to,
            HighlightStyle {
                background_color: Some(bg),
                ..style
            },
        ));
        at = to;
        if range.end > span.end {
            out.push((span.end..range.end, style));
        }
    }
    if at < span.end {
        out.push((at..span.end, plain));
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

/// Drop the oldest transcript lines past [`MAX_PRELUDE_LINES`]; returns how
/// many went.
fn cap_prelude(prelude: &mut Vec<SharedString>) -> usize {
    let excess = prelude.len().saturating_sub(MAX_PRELUDE_LINES);
    if excess > 0 {
        prelude.drain(..excess);
    }
    excess
}

/// A transcript selection after `dropped` lines left the top: rows move up
/// with their text; an end that left is clamped to the first kept cell, and
/// a selection wholly in the dropped lines is gone.
fn shift_selection(
    sel: (Cell2, Cell2, SelectKind),
    dropped: usize,
) -> Option<(Cell2, Cell2, SelectKind)> {
    let (a, b, kind) = sel;
    if a.0.max(b.0) < dropped {
        return None;
    }
    let shift = |(r, c): Cell2| {
        if r < dropped {
            (0, 0)
        } else {
            (r - dropped, c)
        }
    };
    Some((shift(a), shift(b), kind))
}

/// Characters that end a word for a double-click (alacritty's default
/// semantic escape characters, so the transcript and the live session
/// agree).
const WORD_SEPARATORS: &str = ",│`|:\"' ()[]{}<>\t";

/// Char range of the word (or run of separators / blanks) at char `col`.
fn word_span(text: &str, col: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() {
        return (col, col + 1);
    }
    let is_word = |c: char| !WORD_SEPARATORS.contains(c);
    let kind = is_word(chars[col]);
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) == kind && (kind || chars[start - 1] == chars[col])
    {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && is_word(chars[end]) == kind && (kind || chars[end] == chars[col]) {
        end += 1;
    }
    (start, end)
}

/// Ordered selection between `anchor` and `head`, grown to whole words or
/// lines for a double / triple click. `text` gives a row's text.
fn expand_selection(
    anchor: Cell2,
    head: Cell2,
    kind: SelectKind,
    text: impl Fn(usize) -> String,
) -> (Cell2, Cell2) {
    let (start, end) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    match kind {
        SelectKind::Cells => (start, end),
        SelectKind::Word => {
            let (s0, _) = word_span(&text(start.0), start.1);
            let (_, e1) = word_span(&text(end.0), end.1);
            ((start.0, s0), (end.0, e1))
        }
        SelectKind::Line => ((start.0, 0), (end.0, text(end.0).chars().count().max(1))),
    }
}

impl Drop for TerminalPanel {
    fn drop(&mut self) {
        self.stop_shell();
    }
}
impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shell_running = self.shell_running();
        let has_selection = self.selection_span().is_some();
        let selection = self.selection_span();
        let selection_bg = cx.theme().selection;
        // Unique ids per session so split panes do not share hit-testing / focus.
        let eid = cx.entity_id().as_u64();
        // Cell metrics for PTY sizing (mono font advance × line height).
        let rem = window.rem_size();
        let font_size = rem * FONT_REMS;
        let line_h = rem * LINE_HEIGHT_REMS;
        let font_id = window
            .text_system()
            .resolve_font(&gpui_kit::font(cx.theme().mono_font_family.clone()));
        let cell_w = window
            .text_system()
            .advance(font_id, font_size, 'M')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);
        self.cell = (cell_w, line_h);
        self.rem = rem;
        let viewport = self.viewport.clone();
        let colors = TermColors::new(
            cx.theme().is_dark(),
            cx.theme().foreground,
            cx.theme().background,
        );
        let row_count = self.total_rows().max(1);
        let menu_focus = self.input.focus_handle(cx);

        v_flex()
            .id(ElementId::NamedInteger("terminal-panel".into(), eid))
            .size_full()
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &crate::actions::TerminalCopy, _, cx| {
                    this.copy_selection(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::TerminalPaste, _, cx| {
                    this.paste_clipboard(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::TerminalSelectAll, _, cx| {
                    this.select_all(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::TerminalClear, _, cx| this.clear(cx)))
            .child(
                h_flex()
                    .id(ElementId::NamedInteger("terminal-header".into(), eid))
                    .h(px(36.))
                    .px_2()
                    .gap_3()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(if shell_running {
                                "Terminal"
                            } else {
                                "Terminal (shell stopped)"
                            }),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-copy".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Copy))
                            .tooltip("Copy Selection (Ctrl+Shift+C)")
                            .disabled(!has_selection)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_selection(cx))),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-paste".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Clipboard))
                            .tooltip("Paste (Ctrl+Shift+V)")
                            .disabled(!shell_running)
                            .on_click(cx.listener(|this, _, _, cx| this.paste_clipboard(cx))),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-clear".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Eraser))
                            .tooltip("Clear")
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .when(!shell_running, |this| {
                        this.child(
                            Button::new(ElementId::NamedInteger("terminal-restart".into(), eid))
                                .ghost()
                                .small()
                                .icon(Icon::new(IconName::RefreshCw))
                                .label("Restart")
                                .tooltip("Start a new shell in this session")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.restart(window, cx);
                                })),
                        )
                    })
                    .child(div().w(px(1.)).h(px(16.)).bg(cx.theme().border))
                    // VS Code's panel actions: New, Split, Kill, Maximize, Close.
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-new".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Plus))
                            .tooltip("New Terminal (Ctrl+Shift+`)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::actions::NewTerminal), cx)
                            }),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-split".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Columns2))
                            .tooltip("Split Terminal (Ctrl+Shift+5)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::actions::SplitTerminal), cx)
                            }),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-kill".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Trash))
                            .tooltip("Kill Terminal")
                            .disabled(!shell_running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.stop_shell();
                                this.append("— shell stopped\n", cx);
                                this.focus_input(window, cx);
                                // Workspace closes the tab when another session remains.
                                cx.emit(TerminalEvent::CloseSession);
                            })),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-maximize".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(if self.maximized {
                                IconName::Minimize2
                            } else {
                                IconName::Maximize2
                            }))
                            .tooltip(if self.maximized {
                                "Restore Panel Size"
                            } else {
                                "Maximize Panel Size"
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(crate::actions::ToggleMaximizedPanel),
                                    cx,
                                )
                            }),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-close".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::X))
                            .tooltip("Close Panel (Ctrl+`)")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalEvent::HidePanel))),
                    ),
            )
            .child(
                div()
                    .id(ElementId::NamedInteger("terminal-output".into(), eid))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .line_height(rems(LINE_HEIGHT_REMS))
                    // Clicking the transcript focuses the input so typing
                    // resumes without hunting for it.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            // Start a selection (a plain click clears it);
                            // double-click selects a word, triple a line,
                            // Shift+click extends. Following the tail is left
                            // alone: selecting while scrolled up must not
                            // jump to the end.
                            this.begin_selection(
                                event.position,
                                SelectKind::from_click_count(event.click_count),
                                event.modifiers.shift,
                            );
                            this.focus_input(window, cx);
                            // Otherwise the panel's own tracked focus takes the
                            // click after us (bubble phase) and typed text has
                            // no input to land in.
                            window.prevent_default();
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        if this.selecting.is_none()
                            || event.pressed_button != Some(MouseButton::Left)
                        {
                            return;
                        }
                        this.drag_selection(event.position);
                        cx.notify();
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.selecting = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.selecting = None;
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        // Scrolling up pauses follow; scrolling down to the
                        // end resumes it.
                        let dy = match event.delta {
                            ScrollDelta::Pixels(p) => f32::from(p.y),
                            ScrollDelta::Lines(l) => l.y * 16.,
                        };
                        if dy > 0.5 {
                            this.follow_tail = false;
                        } else if dy < -0.5 && this.scroll.is_scrolled_to_end() != Some(false) {
                            this.follow_tail = true;
                        }
                        cx.notify();
                    }))
                    .child(
                        // Virtual list: only the rows in view are fetched from
                        // the emulator and laid out, whatever the scrollback.
                        uniform_list(
                            ElementId::NamedInteger("terminal-rows".into(), eid),
                            row_count,
                            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                                let start = range.start;
                                let cursor = this.cursor_row();
                                let mut texts = this.row_texts(range.clone());
                                texts.resize(range.len(), StyledRow::default());
                                texts
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, line)| {
                                        let ix = start + i;
                                        let cursor_col = match cursor {
                                            Some((cr, cc)) if cr == ix => Some(cc),
                                            _ => None,
                                        };
                                        let selected =
                                            selection.and_then(|((r0, c0), (r1, c1))| {
                                                (r0..=r1).contains(&ix).then(|| {
                                                    let from = if ix == r0 { c0 } else { 0 };
                                                    let to = if ix == r1 { c1 } else { usize::MAX };
                                                    from..to
                                                })
                                            });
                                        let (text, highlights) = row_highlights(
                                            line,
                                            cursor_col,
                                            selected.map(|cols| (cols, selection_bg)),
                                            &colors,
                                        );
                                        div()
                                            .id(ElementId::NamedInteger(
                                                SharedString::from(format!("term-line-{eid}")),
                                                ix as u64,
                                            ))
                                            .h(line_h)
                                            .w_full()
                                            .whitespace_nowrap()
                                            .child(
                                                StyledText::new(text).with_highlights(highlights),
                                            )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    // Records the viewport size for PTY resize (cols × rows).
                    .child(
                        canvas(
                            move |bounds, _, _| viewport.set(Some(bounds.size)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    // VS Code's terminal context menu: Copy (only with a
                    // selection), Paste, Select All, Clear. Ctrl+C stays ^C.
                    .context_menu(move |menu, _, _| {
                        menu.action_context(menu_focus.clone())
                            .item(
                                PopupMenuItem::new("Copy")
                                    .disabled(!has_selection)
                                    .action(Box::new(crate::actions::TerminalCopy)),
                            )
                            .item(
                                PopupMenuItem::new("Paste")
                                    .disabled(!shell_running)
                                    .action(Box::new(crate::actions::TerminalPaste)),
                            )
                            .separator()
                            .item(
                                PopupMenuItem::new("Select All")
                                    .action(Box::new(crate::actions::TerminalSelectAll)),
                            )
                            .item(
                                PopupMenuItem::new("Clear")
                                    .action(Box::new(crate::actions::TerminalClear)),
                            )
                    }),
            )
            // Input row: the command line before a shell runs, the IME
            // composition line while one does (typed text is forwarded as it
            // is committed, so it never lingers here).
            .child(
                h_flex()
                    .id(ElementId::NamedInteger("terminal-input-row".into(), eid))
                    .h(px(36.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.focus_input(window, cx);
                            window.prevent_default();
                        }),
                    )
                    .when(!shell_running, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("$"),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            // Monospace like the output, so typed commands
                            // line up with what the shell prints.
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(Input::new(&self.input).appearance(false)),
                    ),
            )
    }
}

fn default_shell(load_config: bool) -> CommandBuilder {
    #[cfg(windows)]
    {
        let _ = load_config;
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        CommandBuilder::new(shell)
    }
    #[cfg(not(windows))]
    {
        let shell = crate::term_session::user_shell();
        let mut cmd = CommandBuilder::new(&shell);
        let is_bash = shell.ends_with("bash");
        if !load_config {
            // Clean shell: a broken rc cannot kill the session.
            if is_bash {
                cmd.arg("--norc");
                cmd.arg("--noprofile");
            } else if shell.ends_with("zsh") {
                cmd.arg("-f");
            }
            cmd.env("PS1", "$ ");
        }
        // Interactive (prompt, job control, rc), not a login shell — VS
        // Code's Linux default.
        cmd.arg("-i");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "malgel");
        cmd
    }
}

pub fn terminal_cwd(folder: Option<&Path>, doc_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(folder) = folder {
        return Some(folder.to_path_buf());
    }
    if let Some(parent) = doc_path.and_then(|p| p.parent()) {
        return Some(parent.to_path_buf());
    }
    std::env::current_dir().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_background_keeps_run_colors() {
        let red = HighlightStyle {
            color: Some(gpui_kit::red()),
            ..Default::default()
        };
        let bg = gpui_kit::blue();
        // "ab" plain, "cd" red, "ef" plain; select "bcde".
        let out = overlay_background(vec![(2..4, red)], 1..5, bg);
        let ranges: Vec<_> = out.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![1..2, 2..4, 4..5]);
        assert_eq!(out[1].1.color, red.color);
        assert!(out.iter().all(|(_, s)| s.background_color == Some(bg)));
        // Outside the span nothing changes.
        assert_eq!(overlay_background(vec![(0..1, red)], 2..3, bg)[0].0, 0..1);
    }

    #[test]
    fn terminal_cwd_prefers_folder() {
        let folder = Path::new("/tmp/proj");
        let doc = Path::new("/tmp/other/file.md");
        assert_eq!(
            terminal_cwd(Some(folder), Some(doc)).as_deref(),
            Some(folder)
        );
    }

    #[test]
    fn grid_size_from_viewport() {
        // 800x300 px, 8px cells, 20px rows, 16px rem → 1rem padding removed.
        let size = grid_size_for(px(800.), px(300.), px(8.), px(20.), px(16.));
        assert_eq!(size, GridSize { cols: 98, rows: 14 });
        // Tiny panes clamp instead of producing a zero-sized PTY.
        let tiny = grid_size_for(px(10.), px(10.), px(8.), px(20.), px(16.));
        assert_eq!(tiny, GridSize { cols: 20, rows: 4 });
    }

    #[test]
    fn prelude_is_capped() {
        let mut prelude: Vec<SharedString> = (0..5_000).map(|i| i.to_string().into()).collect();
        cap_prelude(&mut prelude);
        assert_eq!(prelude.len(), MAX_PRELUDE_LINES);
        assert_eq!(prelude.last().map(|s| s.as_ref()), Some("4999"));
    }

    fn ks(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn app_chords_skip_the_shell() {
        for chord in [
            "ctrl-`",
            "ctrl-shift-p",
            "ctrl-p",
            "ctrl-tab",
            "ctrl-shift-`",
            "ctrl-q",
        ] {
            assert!(is_app_chord(&ks(chord)), "{chord}");
        }
        for key in [
            "ctrl-c",
            "ctrl-d",
            "ctrl-z",
            "ctrl-r",
            "ctrl-l",
            "ctrl-u",
            "ctrl-w",
            "tab",
            "up",
            "escape",
            "ctrl-shift-c",
            "ctrl-shift-v",
            "alt-b",
        ] {
            assert!(!is_app_chord(&ks(key)), "{key}");
        }
    }
}
