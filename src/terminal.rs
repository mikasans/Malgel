//! Integrated terminal: a bottom panel with a real shell (Ctrl+`).
//!
//! Kept simple: one session, toggle / focus / clear / kill. Not a full
//! terminal IDE (no tabs, splits, or profiles yet).

use std::{
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
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    assets::IconName,
    base::Disableable as _,
    component::{
        ActiveTheme as _, Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        v_flex,
    },
    div, px,
    prelude::FluentBuilder as _,
};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const MAX_OUTPUT_CHARS: usize = 200_000;

enum PtyMessage {
    Output(Vec<u8>),
    Exited,
}

/// One integrated terminal session in the bottom panel.
pub struct TerminalPanel {
    focus_handle: FocusHandle,
    cwd: Option<PathBuf>,
    output: String,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    rx: Option<Receiver<PtyMessage>>,
    _reader: Option<thread::JoinHandle<()>>,
    _pump: Option<gpui_kit::Task<()>>,
}

impl EventEmitter<()> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Run a command…"));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let line = input.read(cx).value().to_string();
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    this.send_line(&line, cx);
                }
            },
        )
        .detach();
        Self {
            focus_handle: cx.focus_handle(),
            cwd: None,
            output: String::new(),
            input,
            scroll: ScrollHandle::new(),
            writer: None,
            child: None,
            rx: None,
            _reader: None,
            _pump: None,
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
        // While a PTY is alive the secondary `$ Run a command…` row is hidden;
        // focus the panel so typed keys go straight to the shell (VS Code feel).
        if self.writer.is_some() {
            self.focus_handle.focus(window, cx);
        } else {
            self.input.focus_handle(cx).focus(window, cx);
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.output.clear();
        cx.notify();
    }

    pub fn ensure_running(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            self.start_shell(cx);
        }
        self.focus_input(window, cx);
        cx.notify();
    }

    pub fn stop_shell(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        self.writer = None;
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
        let mut payload = line.as_bytes().to_vec();
        payload.push(b'\n');
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

    /// Forward a key to the live PTY so the panel can hide the secondary
    /// composer while the shell is running.
    fn forward_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.writer.is_none() {
            return;
        }
        let keys = &event.keystroke;
        // Ctrl+C → ETX so interrupt-from-keyboard still reaches the shell.
        if keys.modifiers.control && !keys.modifiers.alt && !keys.modifiers.platform {
            if keys.key.eq_ignore_ascii_case("c") {
                self.write_bytes(&[0x03]);
                cx.stop_propagation();
                return;
            }
            // Leave other Ctrl/Cmd chords to the workspace (Ctrl+`, etc.).
            return;
        }
        if keys.modifiers.platform || keys.modifiers.secondary() {
            return;
        }
        match keys.key.as_str() {
            "enter" => {
                self.write_bytes(b"\n");
                cx.stop_propagation();
            }
            "backspace" => {
                self.write_bytes(&[0x7f]);
                cx.stop_propagation();
            }
            "tab" => {
                self.write_bytes(b"\t");
                cx.stop_propagation();
            }
            "escape" => {
                self.write_bytes(&[0x1b]);
                cx.stop_propagation();
            }
            _ => {
                if let Some(ch) = keys.key_char.as_deref() {
                    self.write_bytes(ch.as_bytes());
                    cx.stop_propagation();
                } else if keys.key.len() == 1 && !keys.modifiers.control {
                    self.write_bytes(keys.key.as_bytes());
                    cx.stop_propagation();
                }
            }
        }
    }

    fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        self.output.push_str(text);
        self.trim_output();
        cx.notify();
    }

    fn trim_output(&mut self) {
        if self.output.len() <= MAX_OUTPUT_CHARS {
            return;
        }
        let keep = MAX_OUTPUT_CHARS / 2;
        let start = self
            .output
            .char_indices()
            .nth(self.output.chars().count().saturating_sub(keep))
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.output = self.output[start..].to_string();
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
        self.child = Some(child);
        self.rx = Some(rx);
        self._reader = Some(reader);

        let entity = cx.weak_entity();
        self._pump = Some(cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
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
        let Some(rx) = &self.rx else {
            return false;
        };
        let mut changed = false;
        let mut exited = false;
        loop {
            match rx.try_recv() {
                Ok(PtyMessage::Output(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.output.push_str(&strip_ansi(&text));
                    changed = true;
                }
                Ok(PtyMessage::Exited) => {
                    self.output.push_str("\n— shell exited\n");
                    exited = true;
                    changed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    exited = true;
                    break;
                }
            }
        }
        if exited {
            self.writer = None;
            self.child = None;
            self.rx = None;
        }
        if changed {
            self.trim_output();
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
        let output: SharedString = self.output.clone().into();
        let shell_running = self.writer.is_some();
        v_flex()
            .id("terminal-panel")
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
                    .id("terminal-header")
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
                            .child("Terminal"),
                    )
                    // Labeled + small (not xsmall/compact) so Clear and Kill share a
                    // predictable hit area — icon-only X was easy to miss next to Trash.
                    .child(
                        Button::new("terminal-clear")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Trash))
                            .label("Clear")
                            .tooltip("Clear terminal output")
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .child(
                        Button::new("terminal-kill")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::X))
                            .label("Kill")
                            .tooltip("Kill the shell process")
                            .disabled(!shell_running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.stop_shell();
                                this.append("— shell stopped\n", cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("terminal-output")
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().w_full().child(output)),
            )
            // One input surface: while the PTY is alive, hide the secondary
            // `$ Run a command…` composer and type into the shell prompt via
            // the panel (VS Code). Show the composer only to start a shell.
            .when(!shell_running, |this| {
                this.child(
                    h_flex()
                        .id("terminal-input-row")
                        .h(px(36.))
                        .px_2()
                        .gap_2()
                        .items_center()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("$"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.input).appearance(false)),
                        ),
                )
            })
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
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-i");
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
        } else if ch != '\r' {
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
    fn terminal_cwd_prefers_folder() {
        let folder = Path::new("/tmp/proj");
        let doc = Path::new("/tmp/other/file.md");
        assert_eq!(
            terminal_cwd(Some(folder), Some(doc)).as_deref(),
            Some(folder)
        );
    }
}
