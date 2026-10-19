//! Integrated terminal: a bottom panel with a real shell (Ctrl+`).
//!
//! Open/focus/refresh follow VS Code / Pebrel-style panel habits: summoning
//! shows the panel and focuses the composer when focus is elsewhere, and only
//! hides when the terminal already owns focus. Output auto-scrolls to the end
//! while the user is following the tail; scrolling up pauses follow. New
//! Terminal is Ctrl+Shift+` (never the same chord as toggle).
//!
//! Output is a line buffer (not one wrapping string) so glyph columns stay
//! aligned with the shell cursor: `\r` rewrites the current line, tabs expand
//! to spaces, and each row is `whitespace_nowrap` at a fixed line height.
//! Copy / paste use Ctrl+Shift+C / Ctrl+Shift+V (VS Code), leaving Ctrl+C as
//! interrupt. A single line composer always owns typed input (IME-safe) — the
//! panel key sink only handles copy/paste/interrupt chords so Escape/`^[`
//! cannot bleed into the PTY. While the PTY is alive its `$` prefix is omitted
//! so the shell prompt in the transcript is the only dollar — no double
//! composers. Element ids are unique per session so split panes do not share
//! hit-testing. Kill closes the session tab when more than one exists; the
//! last session shows a Restart affordance instead of a zombie empty tab.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::Duration,
};

use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _,
    Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window,
    assets::IconName,
    base::Disableable as _,
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        v_flex,
    },
    div, point,
    prelude::FluentBuilder as _,
    px, rems,
};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

const MAX_OUTPUT_CHARS: usize = 200_000;
/// Cap painted rows so a flooded PTY cannot rebuild tens of thousands of
/// elements every frame. Copy / trim still see the full ring buffer.
const MAX_RENDER_LINES: usize = 2_000;
const TAB_WIDTH: usize = 8;
const LINE_HEIGHT_REMS: f32 = 1.35;

enum PtyMessage {
    Output(Vec<u8>),
    Exited,
}

/// One integrated terminal session in the bottom panel.
pub struct TerminalPanel {
    focus_handle: FocusHandle,
    cwd: Option<PathBuf>,
    /// Completed lines (no trailing newline) plus the live line under edit.
    lines: VecDeque<String>,
    /// Characters of the unfinished last line (after the last `\n`).
    current: String,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    /// Kept alive so the PTY master FD is not closed while reader/writer clones run.
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    rx: Option<Receiver<PtyMessage>>,
    _reader: Option<thread::JoinHandle<()>>,
    _pump: Option<gpui_kit::Task<()>>,
    /// Stick output viewport to the end while the user has not scrolled up.
    follow_tail: bool,
}

/// Events the workspace listens for (Kill → close session tab).
pub enum TerminalEvent {
    /// User asked to kill this session (toolbar Kill). Workspace closes the tab
    /// when another session remains; otherwise leaves a Restart affordance.
    CloseSession,
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
            |this, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let line = input.read(cx).value().to_string();
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    // Line composer: start shell if needed, then send CR-terminated line.
                    this.send_line(&line, cx);
                }
            },
        )
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            cwd: None,
            lines: VecDeque::new(),
            current: String::new(),
            input,
            scroll: ScrollHandle::new(),
            writer: None,
            _master: None,
            child: None,
            rx: None,
            _reader: None,
            _pump: None,
            follow_tail: true,
        }
    }

    pub fn set_cwd(&mut self, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        if self.writer.is_some() {
            self.stop_shell();
            self.start_shell(cx);
        }
        cx.notify();
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        // Focus the Input key-sink so the window has an input_handler.
        // X11 IME commits (and `xdotool type`) are dropped without one; raw
        // KeyDown alone is not enough for typed characters under Xvfb.
        let running = self.writer.is_some();
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
        self.start_shell(cx);
        self.follow_tail = true;
        self.scroll_to_end();
        self.focus_input(window, cx);
        cx.notify();
    }

    /// Keep the viewport on the newest output (Pebrel/VS Code follow-tail).
    fn scroll_to_end(&self) {
        // Large negative Y; GPUI clamps to the content bottom.
        self.scroll.set_offset(point(px(0.), px(-1_000_000.)));
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.lines.clear();
        self.current.clear();
        self.follow_tail = true;
        self.scroll_to_end();
        cx.notify();
    }

    /// Full transcript for clipboard copy.
    fn transcript(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&self.current);
        out
    }

    fn copy_all(&self, cx: &mut Context<Self>) {
        let text = self.transcript();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        // Normalize to CR — what a keyboard Enter sends into a tty.
        let payload = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', "\r");
        self.write_bytes(payload.as_bytes());
    }

    pub fn ensure_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            self.start_shell(cx);
        }
        // Re-arm follow-tail so opening via shortcut always shows latest output.
        self.follow_tail = true;
        self.scroll_to_end();
        self.focus_input(window, cx);
        cx.notify();
    }

    pub fn stop_shell(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        self.writer = None;
        self._master = None;
        self.rx = None;
        self._reader = None;
        self._pump = None;
    }

    fn send_line(&mut self, line: &str, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            self.start_shell(cx);
        }
        let Some(writer) = &self.writer else {
            self.append("Couldn’t start a shell.\n", cx);
            return;
        };
        // PTY line discipline expects CR (same as a real keyboard Enter).
        let mut payload = line.as_bytes().to_vec();
        payload.push(b'\r');
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(&payload);
            let _ = w.flush();
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let Some(writer) = &self.writer else {
            return;
        };
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Chord-only key sink. Printable input and Escape stay on the line
    /// composer so split-pane focus cannot inject `^[` into the wrong PTY.
    fn forward_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            return;
        }
        let keys = &event.keystroke;
        // Ctrl+Shift+C / Ctrl+Shift+V — copy / paste (VS Code terminal).
        if keys.modifiers.control && keys.modifiers.shift && !keys.modifiers.alt {
            if keys.key.eq_ignore_ascii_case("c") {
                self.copy_all(cx);
                cx.stop_propagation();
                return;
            }
            if keys.key.eq_ignore_ascii_case("v") {
                self.paste_clipboard(cx);
                cx.stop_propagation();
                return;
            }
        }
        // Ctrl+C → ETX so interrupt-from-keyboard still reaches the shell.
        if keys.modifiers.control
            && !keys.modifiers.alt
            && !keys.modifiers.platform
            && !keys.modifiers.shift
            && keys.key.eq_ignore_ascii_case("c")
        {
            self.write_bytes(&[0x03]);
            cx.stop_propagation();
        }
        #[cfg(target_os = "macos")]
        {
            if keys.modifiers.platform && !keys.modifiers.alt && !keys.modifiers.control {
                if keys.key.eq_ignore_ascii_case("c") {
                    self.copy_all(cx);
                    cx.stop_propagation();
                    return;
                }
                if keys.key.eq_ignore_ascii_case("v") {
                    self.paste_clipboard(cx);
                    cx.stop_propagation();
                }
            }
        }
    }

    pub fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ingest(text);
        self.trim_output();
        if self.follow_tail {
            self.scroll_to_end();
        }
        cx.notify();
    }

    /// Feed PTY / status text into the line buffer with `\r` / tab handling.
    fn ingest(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\n' => {
                    self.lines.push_back(std::mem::take(&mut self.current));
                }
                '\r' => {
                    // CRLF from a tty: the CR is just the line-ending prefix —
                    // do not clear, or the echoed command vanishes before `\n`.
                    if chars.peek() == Some(&'\n') {
                        continue;
                    }
                    // Bare CR (progress / spinner): rewrite from column 0.
                    self.current.clear();
                }
                '\t' => {
                    let col = self.current.chars().count();
                    let spaces = TAB_WIDTH - (col % TAB_WIDTH);
                    for _ in 0..spaces {
                        self.current.push(' ');
                    }
                }
                c if c == '\u{08}' || c == '\u{7f}' => {
                    self.current.pop();
                }
                // Drop other C0 controls (BEL, etc.) so they cannot shift glyphs.
                c if c.is_control() => {}
                c => self.current.push(c),
            }
        }
    }

    fn trim_output(&mut self) {
        let mut total: usize = self.lines.iter().map(|l| l.len() + 1).sum();
        total += self.current.len();
        if total <= MAX_OUTPUT_CHARS {
            return;
        }
        while total > MAX_OUTPUT_CHARS / 2 {
            let Some(removed) = self.lines.pop_front() else {
                break;
            };
            total = total.saturating_sub(removed.len() + 1);
        }
    }

    fn start_shell(&mut self, cx: &mut Context<Self>) {
        self.stop_shell();
        let pty_system = NativePtySystem::default();
        let pair = match pty_system.openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(err) => {
                self.append(&format!("PTY error: {err}\n"), cx);
                return;
            }
        };

        let mut cmd = default_shell();
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        let child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(err) => {
                self.append(&format!("Couldn’t start shell: {err}\n"), cx);
                return;
            }
        };
        drop(pair.slave);

        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(err) => {
                self.append(&format!("Couldn’t read shell output: {err}\n"), cx);
                return;
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(err) => {
                self.append(&format!("Couldn’t write to shell: {err}\n"), cx);
                return;
            }
        };

        let (tx, rx) = mpsc::channel();
        let reader_tx = tx;
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if reader_tx
                            .send(PtyMessage::Output(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = reader_tx.send(PtyMessage::Exited);
        });

        self.writer = Some(writer);
        self._master = Some(pair.master);
        self.child = Some(child);
        self.rx = Some(rx);
        self._reader = Some(reader);

        let entity = cx.weak_entity();
        self._pump = Some(cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let cont = entity
                    .update(cx, |this, cx| this.drain_pty(cx))
                    .unwrap_or(false);
                if !cont {
                    break;
                }
            }
        }));

        let cwd_label = self
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".into());
        self.append(&format!("— shell in {cwd_label}\n"), cx);
    }

    /// Returns whether to keep pumping (channel still open).
    fn drain_pty(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(rx) = self.rx.take() else {
            return false;
        };
        let mut changed = false;
        let mut exited = false;
        let mut chunks = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(PtyMessage::Output(bytes)) => chunks.push(bytes),
                Ok(PtyMessage::Exited) => {
                    exited = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    exited = true;
                    break;
                }
            }
        }
        for bytes in chunks {
            let text = String::from_utf8_lossy(&bytes);
            self.ingest(&strip_ansi(&text));
            changed = true;
        }
        if exited {
            self.ingest(
                "
— shell exited
",
            );
            changed = true;
            self.writer = None;
            self._master = None;
            self.child = None;
            self._pump = None;
            self._reader = None;
        } else {
            self.rx = Some(rx);
        }
        if changed {
            self.trim_output();
            if self.follow_tail {
                self.scroll_to_end();
            }
            cx.notify();
        }
        self.rx.is_some()
    }
}

impl Drop for TerminalPanel {
    fn drop(&mut self) {
        self.stop_shell();
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shell_running = self.writer.is_some();
        // Unique ids per session so split panes do not share hit-testing / focus.
        let eid = cx.entity_id().as_u64();
        let skip = self.lines.len().saturating_sub(MAX_RENDER_LINES);
        let mut rows: Vec<SharedString> = self
            .lines
            .iter()
            .skip(skip)
            .map(|l| SharedString::from(l.clone()))
            .collect();
        // Always show the live line (may be empty) so the cursor row stays put.
        rows.push(SharedString::from(self.current.clone()));

        v_flex()
            .id(ElementId::NamedInteger("terminal-panel".into(), eid))
            .size_full()
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.forward_key(event, cx);
            }))
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
                            .label("Copy")
                            .tooltip("Copy all output (Ctrl+Shift+C)")
                            .on_click(cx.listener(|this, _, _, cx| this.copy_all(cx))),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-paste".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Clipboard))
                            .label("Paste")
                            .tooltip("Paste into shell (Ctrl+Shift+V)")
                            .disabled(!shell_running)
                            .on_click(cx.listener(|this, _, _, cx| this.paste_clipboard(cx))),
                    )
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-clear".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Trash))
                            .label("Clear")
                            .tooltip("Clear terminal output")
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
                    .child(
                        Button::new(ElementId::NamedInteger("terminal-kill".into(), eid))
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::X))
                            .label("Kill")
                            .tooltip("Kill the shell and close this session (VS Code)")
                            .disabled(!shell_running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.stop_shell();
                                this.append("— shell stopped\n", cx);
                                this.focus_input(window, cx);
                                // Workspace closes the tab when another session remains.
                                cx.emit(TerminalEvent::CloseSession);
                            })),
                    ),
            )
            .child(
                div()
                    .id(ElementId::NamedInteger("terminal-output".into(), eid))
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .line_height(rems(LINE_HEIGHT_REMS))
                    .overflow_x_scroll()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    // Clicking the transcript focuses the composer so typing
                    // resumes without hunting for it (Pebrel/VS Code).
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.follow_tail = true;
                            this.focus_input(window, cx);
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        // Scrolling up pauses follow; nudging down near the end resumes.
                        // Bind to this surface so a focused terminal (not an image
                        // tab above) owns the wheel when the pointer is here.
                        let dy = match event.delta {
                            ScrollDelta::Pixels(p) => f32::from(p.y),
                            ScrollDelta::Lines(l) => l.y * 16.,
                        };
                        if dy > 0.5 {
                            this.follow_tail = false;
                        } else if dy < -0.5 {
                            this.follow_tail = true;
                            this.scroll_to_end();
                        }
                        cx.notify();
                    }))
                    .child(v_flex().w_full().children(rows.into_iter().enumerate().map(
                        move |(ix, line)| {
                            // Non-breaking space keeps an empty line's height so
                            // the cursor row does not collapse and drift.
                            let shown = if line.is_empty() {
                                SharedString::from("\u{00a0}")
                            } else {
                                line
                            };
                            div()
                                .id(ElementId::NamedInteger(
                                    SharedString::from(format!("term-line-{eid}")),
                                    ix as u64,
                                ))
                                .w_full()
                                .whitespace_nowrap()
                                .child(shown)
                        },
                    ))),
            )
            // Single line composer (always). While the PTY is alive we omit the
            // `$` prefix so the shell prompt in the transcript is the only dollar
            // (no double composers). The Input keeps the IME input_handler.
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
                            .child(Input::new(&self.input).appearance(false)),
                    ),
            )
    }
}

fn default_shell() -> CommandBuilder {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        CommandBuilder::new(shell)
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        // Skip rc/profile so a broken ~/.bashrc cannot kill the session under
        // headless / Xvfb runs; still interactive for a prompt + job control.
        if shell.ends_with("/bash") || shell.ends_with("bash") {
            cmd.arg("--norc");
            cmd.arg("--noprofile");
        }
        cmd.arg("-i");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("PS1", "$ ");
        cmd
    }
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            }
        } else {
            // Keep `\r` / `\n` / tabs — `ingest` interprets them.
            out.push(ch);
        }
    }
    out
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
    fn strip_ansi_removes_colors_and_keeps_text() {
        assert_eq!(strip_ansi("\u{1b}[32mhi\u{1b}[0m there"), "hi there");
    }

    #[test]
    fn strip_ansi_keeps_carriage_return() {
        assert!(strip_ansi("a\rb").contains('\r'));
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
    fn trim_output_pops_from_front_without_shifting() {
        let mut lines: VecDeque<String> = (0..500)
            .map(|i| format!("line-{i}-{}", "y".repeat(200)))
            .collect();
        let mut total: usize = lines.iter().map(|l| l.len() + 1).sum();
        assert!(total > MAX_OUTPUT_CHARS / 4);
        while total > MAX_OUTPUT_CHARS / 2 {
            let Some(removed) = lines.pop_front() else {
                break;
            };
            total = total.saturating_sub(removed.len() + 1);
        }
        assert!(total <= MAX_OUTPUT_CHARS / 2 + 400);
        assert!(!lines.is_empty());
    }

    #[test]
    fn ingest_cr_rewrites_current_line() {
        let mut lines = Vec::new();
        let mut current = String::new();
        let ingest = |text: &str, lines: &mut Vec<String>, current: &mut String| {
            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    '\n' => lines.push(std::mem::take(current)),
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            continue;
                        }
                        current.clear();
                    }
                    '\t' => {
                        let col = current.chars().count();
                        let spaces = TAB_WIDTH - (col % TAB_WIDTH);
                        for _ in 0..spaces {
                            current.push(' ');
                        }
                    }
                    c => current.push(c),
                }
            }
        };
        ingest("hello\rworld", &mut lines, &mut current);
        assert!(lines.is_empty());
        assert_eq!(current, "world");
        ingest("\nnext", &mut lines, &mut current);
        assert_eq!(lines, vec!["world".to_string()]);
        assert_eq!(current, "next");
    }

    #[test]
    fn ingest_crlf_keeps_echoed_command() {
        let mut lines: VecDeque<String> = VecDeque::new();
        let mut current = String::new();
        let ingest = |text: &str, lines: &mut VecDeque<String>, current: &mut String| {
            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    '\n' => lines.push_back(std::mem::take(current)),
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            continue;
                        }
                        current.clear();
                    }
                    c => current.push(c),
                }
            }
        };
        // What a tty typically echoes after we write `echo hi\r`.
        ingest("echo hi\r\nSPRINT\r\n$ ", &mut lines, &mut current);
        assert_eq!(
            lines.iter().cloned().collect::<Vec<_>>(),
            vec!["echo hi".to_string(), "SPRINT".to_string()]
        );
        assert_eq!(current, "$ ");
    }
}
