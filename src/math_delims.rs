//! Convert LaTeX math delimiters to the dollar forms the preview renders:
//! `\[ … \]` becomes a `$$` block (each `$$` on its own line) and `\( … \)`
//! becomes `$…$` on the same line. Fenced code, inline code spans and
//! escaped backslashes (`\\[`, a LaTeX line break) are left alone, as are
//! `\left[`, `\right]`, `\{` and `\}`, which are not delimiters.

use std::ops::Range;

use crate::format::Edit;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    DisplayOpen,
    DisplayClose,
    InlineOpen,
    InlineClose,
}

/// Byte ranges of fenced code blocks and inline code spans.
fn code_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut fence: Option<(u8, usize, usize)> = None; // (char, len, start)
    let mut para_start = 0; // where inline code may start scanning
    let mut pos = 0;
    let mut inline_chunks: Vec<Range<usize>> = Vec::new();
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let trimmed = line.trim_start_matches([' ', '\t']);
        let run = |c: u8| trimmed.bytes().take_while(|&b| b == c).count();
        match fence {
            Some((c, len, fstart)) => {
                let n = run(c);
                if n >= len && trimmed[n..].trim().is_empty() {
                    ranges.push(fstart..pos);
                    fence = None;
                    para_start = pos;
                }
            }
            None => {
                let (c, n) = match trimmed.as_bytes().first() {
                    Some(b'`') => (b'`', run(b'`')),
                    Some(b'~') => (b'~', run(b'~')),
                    _ => (0, 0),
                };
                // A backtick fence's info string has no backticks.
                if n >= 3 && (c == b'~' || !trimmed[n..].contains('`')) {
                    if para_start < start {
                        inline_chunks.push(para_start..start);
                    }
                    fence = Some((c, n, start));
                } else if line.trim().is_empty() {
                    if para_start < start {
                        inline_chunks.push(para_start..start);
                    }
                    para_start = pos;
                }
            }
        }
    }
    match fence {
        // An unclosed fence runs to the end, as in CommonMark.
        Some((_, _, fstart)) => ranges.push(fstart..text.len()),
        None if para_start < text.len() => inline_chunks.push(para_start..text.len()),
        None => {}
    }
    for chunk in inline_chunks {
        code_spans(text, chunk, &mut ranges);
    }
    ranges.sort_by_key(|r| r.start);
    ranges
}

/// Inline code spans within one paragraph: a run of N backticks up to the
/// next run of exactly N.
fn code_spans(text: &str, chunk: Range<usize>, out: &mut Vec<Range<usize>>) {
    let bytes = text.as_bytes();
    let mut i = chunk.start;
    while i < chunk.end {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open = i;
        while i < chunk.end && bytes[i] == b'`' {
            i += 1;
        }
        let n = i - open;
        let mut j = i;
        let mut close = None;
        while j < chunk.end {
            if bytes[j] == b'`' {
                let s = j;
                while j < chunk.end && bytes[j] == b'`' {
                    j += 1;
                }
                if j - s == n {
                    close = Some(j);
                    break;
                }
            } else {
                j += 1;
            }
        }
        if let Some(end) = close {
            out.push(open..end);
            i = end;
        }
    }
}

/// The delimiters outside code, each with its byte offset (of the `\`).
fn tokens(text: &str) -> Vec<(usize, Kind)> {
    let code = code_ranges(text);
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut code_ix = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        while code_ix < code.len() && code[code_ix].end <= i {
            code_ix += 1;
        }
        if let Some(r) = code.get(code_ix)
            && r.start <= i
        {
            i = r.end;
            continue;
        }
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        // `\\` is an escaped backslash (or LaTeX's `\\[2pt]`): skip both.
        let kind = match bytes[i + 1] {
            b'[' => Some(Kind::DisplayOpen),
            b']' => Some(Kind::DisplayClose),
            b'(' => Some(Kind::InlineOpen),
            b')' => Some(Kind::InlineClose),
            _ => None,
        };
        if let Some(kind) = kind {
            out.push((i, kind));
        }
        i += 2;
    }
    out
}

/// Whether `\[ … \]` or `\( … \)` math outside code is in `text`.
pub fn has_bracket_math(text: &str) -> bool {
    (text.contains("\\[") || text.contains("\\(")) && !pairs(text, 0..text.len()).is_empty()
}

/// Matched open/close pairs, both inside `within`. Inline math does not
/// cross a blank line.
fn pairs(text: &str, within: Range<usize>) -> Vec<(usize, usize, bool)> {
    let toks = tokens(text);
    let mut out = Vec::new();
    let mut k = 0;
    while k < toks.len() {
        let (open, kind) = toks[k];
        let (close_kind, display) = match kind {
            Kind::DisplayOpen => (Kind::DisplayClose, true),
            Kind::InlineOpen => (Kind::InlineClose, false),
            _ => {
                k += 1;
                continue;
            }
        };
        let found = toks[k + 1..]
            .iter()
            .position(|&(_, kk)| kk == close_kind || kk == kind);
        match found.map(|p| (p + k + 1, toks[p + k + 1])) {
            Some((ix, (close, kk))) if kk == close_kind => {
                let blank = !display && has_blank_line(&text[open..close]);
                if !blank && open >= within.start && close + 2 <= within.end {
                    out.push((open, close, display));
                }
                k = if blank { k + 1 } else { ix + 1 };
            }
            _ => k += 1,
        }
    }
    out
}

/// A whitespace-only line strictly inside `s` (a paragraph break).
fn has_blank_line(s: &str) -> bool {
    let lines: Vec<&str> = s.split('\n').collect();
    lines.len() > 2
        && lines[1..lines.len() - 1]
            .iter()
            .any(|l| l.trim().is_empty())
}

/// The replacements, in order, as (range, text).
fn replacements(text: &str, within: Range<usize>) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    for (open, close, display) in pairs(text, within) {
        if !display {
            out.push((open..open + 2, "$".to_string()));
            out.push((close..close + 2, "$".to_string()));
            continue;
        }
        let line_start = text[..open].rfind('\n').map_or(0, |n| n + 1);
        let line = &text[line_start..];
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        out.push(display_delim(text, open, &indent));
        out.push(display_delim(text, close, &indent));
    }
    out
}

/// `\[` or `\]` at `at` as `$$` alone on its line: text before it on the
/// line, or after it, moves to a line of its own (same indent).
fn display_delim(text: &str, at: usize, indent: &str) -> (Range<usize>, String) {
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let line_end = text[at..].find('\n').map_or(text.len(), |n| at + n);
    let before = &text[line_start..at];
    let after = &text[at + 2..line_end];
    let mut start = at;
    let mut end = at + 2;
    let mut out = String::new();
    if !before.trim().is_empty() {
        start = line_start + before.trim_end().len();
        out.push('\n');
        out.push_str(indent);
    }
    out.push_str("$$");
    if !after.trim().is_empty() {
        end = at + 2 + (after.len() - after.trim_start().len());
        out.push('\n');
        out.push_str(indent);
    }
    (start..end, out)
}

/// The whole text with every delimiter pair converted.
#[cfg(test)]
pub fn convert(text: &str) -> String {
    apply(text, &replacements(text, 0..text.len()))
}

fn apply(text: &str, reps: &[(Range<usize>, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (range, with) in reps {
        out.push_str(&text[last..range.start]);
        out.push_str(with);
        last = range.end;
    }
    out.push_str(&text[last..]);
    out
}

/// The conversion as one edit spanning the changed text: of the selection
/// when there is one (pairs wholly inside it), otherwise of the document.
/// `None` when there is nothing to convert.
pub fn convert_edit(text: &str, selection: Range<usize>) -> Option<Edit> {
    let within = if selection.is_empty() {
        0..text.len()
    } else {
        selection.clone()
    };
    let reps = replacements(text, within);
    let (first, last) = (reps.first()?, reps.last()?);
    let range = first.0.start..last.0.end;
    let shifted: Vec<_> = reps
        .iter()
        .map(|(r, s)| (r.start - range.start..r.end - range.start, s.clone()))
        .collect();
    let replaced = apply(&text[range.clone()], &shifted);
    let map = |pos: usize| {
        let mut delta: isize = 0;
        for (r, s) in &reps {
            if r.end <= pos {
                delta += s.len() as isize - r.len() as isize;
            } else if r.start < pos {
                return (r.start as isize + delta) as usize + s.len();
            }
        }
        (pos as isize + delta) as usize
    };
    let selection = map(selection.start)..map(selection.end);
    Some(Edit {
        range,
        text: replaced,
        selection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_edit(text: &str, edit: &Edit) -> String {
        format!(
            "{}{}{}",
            &text[..edit.range.start],
            edit.text,
            &text[edit.range.end..]
        )
    }

    #[test]
    fn display_math_becomes_a_dollar_block_on_its_own_lines() {
        assert_eq!(convert("a\n\\[\nx^2\n\\]\nb\n"), "a\n$$\nx^2\n$$\nb\n");
        assert_eq!(convert("\\[ x^2 \\]\n"), "$$\nx^2\n$$\n");
        assert_eq!(
            convert("Then \\[\\boxed{G_1>G_2}\\] holds.\n"),
            "Then\n$$\n\\boxed{G_1>G_2}\n$$\nholds.\n"
        );
        assert_eq!(convert("  \\[ a \\]\n"), "  $$\n  a\n  $$\n");
    }

    #[test]
    fn inline_math_becomes_single_dollars_without_added_spaces() {
        assert_eq!(
            convert("where \\(\\mathcal G\\) is"),
            "where $\\mathcal G$ is"
        );
        assert_eq!(convert("\\( x \\)"), "$ x $");
        assert_eq!(
            convert("\\(a\\), \\(b_1\\) and \\(c\\)\n"),
            "$a$, $b_1$ and $c$\n"
        );
        assert_eq!(
            convert("\\(a\\) then \\[b\\] then \\(c\\)\n"),
            "$a$ then\n$$\nb\n$$\nthen $c$\n"
        );
    }

    #[test]
    fn code_escapes_and_latex_brackets_are_left_alone() {
        let fenced = "```\n\\[x\\] \\(y\\)\n```\n~~~tex\n\\(z\\)\n~~~\n";
        assert_eq!(convert(fenced), fenced);
        let spans = "use `\\(x\\)` or ``a ` \\[y\\]`` here\n";
        assert_eq!(convert(spans), spans);
        let escaped = "a \\\\[2pt] b \\\\(c\\\\)\n";
        assert_eq!(convert(escaped), escaped);
        let latex = "$$\n\\left[ x \\right] \\{0,1\\}\n$$\n";
        assert_eq!(convert(latex), latex);
        assert!(!has_bracket_math(fenced));
        assert!(!has_bracket_math(latex));
        assert!(has_bracket_math("so \\(x\\)"));
        // Outside the code, the same document still converts.
        assert_eq!(
            convert("```\n\\(x\\)\n```\n\\(y\\)\n"),
            "```\n\\(x\\)\n```\n$y$\n"
        );
        assert_eq!(
            convert("\\[\n\\left[ a \\right] \\\\[2pt]\n\\]\n"),
            "$$\n\\left[ a \\right] \\\\[2pt]\n$$\n"
        );
    }

    #[test]
    fn unmatched_delimiters_and_paragraph_breaks_are_kept() {
        assert_eq!(convert("a \\( b\n\nc \\) d"), "a \\( b\n\nc \\) d");
        assert_eq!(convert("only \\]"), "only \\]");
        assert_eq!(convert("\\(a\nb\\)"), "$a\nb$");
    }

    #[test]
    fn a_selection_converts_only_the_math_inside_it() {
        let text = "\\(a\\) \\(b\\) \\(c\\)";
        let start = text.find("\\(b").unwrap();
        let end = start + "\\(b\\)".len();
        let edit = convert_edit(text, start..end).unwrap();
        let out = apply_edit(text, &edit);
        assert_eq!(out, "\\(a\\) $b$ \\(c\\)");
        assert_eq!(&out[edit.selection.clone()], "$b$");
        assert!(convert_edit("no math", 0..0).is_none());
    }

    #[test]
    fn the_reference_document_converts_to_its_dollar_version() {
        let dollar = include_str!("../tests/fixtures/math-dollar.md");
        let brackets = include_str!("../tests/fixtures/math-brackets.md");
        assert_eq!(convert(brackets), dollar);
        // Converting again changes nothing.
        assert_eq!(convert(dollar), dollar);
        let edit = convert_edit(brackets, 0..0).unwrap();
        assert_eq!(apply_edit(brackets, &edit), dollar);
    }

    struct Fixture(gpui_kit::Entity<gpui_kit::component::input::EditorState>);

    impl gpui_kit::Render for Fixture {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            _: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{Styled as _, div, prelude::*};
            div()
                .size_full()
                .child(gpui_kit::component::input::Editor::new(&self.0).size_full())
        }
    }

    /// The conversion is one undo step in the real editor: one Ctrl+Z
    /// restores the original text, and the selected part alone converts.
    #[gpui_kit::test]
    fn one_undo_restores_the_original(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{AppContext as _, test::TestWindowExt as _};
        let original = "a \\(x\\) and \\(y\\)\n\\[\n\\boxed{z}\n\\]\n";
        cx.update(gpui_kit::init);
        let (handle, state) = cx.update(|cx| {
            let bounds = gpui_kit::Bounds {
                origin: gpui_kit::Point::default(),
                size: gpui_kit::size(gpui_kit::px(800.), gpui_kit::px(480.)),
            };
            let (window, view) = gpui_kit::open_window(
                gpui_kit::WindowOptions {
                    window_bounds: Some(gpui_kit::WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        Fixture(cx.new(|cx| {
                            gpui_kit::component::input::EditorState::new(window, cx)
                                .language("markdown")
                                .default_value(original)
                        }))
                    })
                },
            )
            .unwrap();
            (window, view.read(cx).0.clone())
        });
        let undo = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        cx.update_window(handle, |_, window, cx| {
            window.click(("input", state.entity_id()), cx);
            for selection in [0..0, 2..7] {
                let edit = convert_edit(original, selection).unwrap();
                state.update(cx, |editor, cx| {
                    editor.set_selected_range(edit.range.clone(), cx);
                    editor.replace(edit.text, window, cx);
                    editor.set_selected_range(edit.selection, cx);
                });
                assert_ne!(state.read(cx).value(), original);
                window.press(undo, cx);
                assert_eq!(state.read(cx).value(), original);
            }
            let edit = convert_edit(original, 2..7).unwrap();
            assert_eq!(
                apply_edit(original, &edit),
                "a $x$ and \\(y\\)\n\\[\n\\boxed{z}\n\\]\n"
            );
        })
        .unwrap();
    }
}
