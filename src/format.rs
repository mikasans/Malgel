//! Markdown formatting commands as pure text transforms.
//!
//! Each command reads the document and the current selection (byte offsets)
//! and returns one [`Edit`]: the range to replace, its replacement, and the
//! selection to restore afterwards. Keeping them pure makes them easy to test
//! and keeps the editor integration to a single replace call.

use std::ops::Range;

/// A single replacement produced by a formatting command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
    /// The selection after the edit, in post-edit byte offsets.
    pub selection: Range<usize>,
}

/// An inline style that wraps text in a symmetric marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Italic,
    Strikethrough,
    Code,
}

impl Inline {
    fn marker(self) -> &'static str {
        match self {
            Inline::Bold => "**",
            Inline::Italic => "_",
            Inline::Strikethrough => "~~",
            Inline::Code => "`",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Inline::Bold => "bold text",
            Inline::Italic => "italic text",
            Inline::Strikethrough => "struck text",
            Inline::Code => "code",
        }
    }
}

/// A block style applied to the start of every selected line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Heading(u8),
    Quote,
    BulletList,
    NumberedList,
    TaskList,
}

/// Toggle an inline style on the selection.
///
/// A selection already wrapped in the marker, either inside or immediately
/// around it, is unwrapped. An empty selection inside a word styles the
/// word; elsewhere a placeholder is inserted and selected so typing
/// replaces it.
pub fn toggle_inline(text: &str, selection: Range<usize>, style: Inline) -> Edit {
    let marker = style.marker();
    let selection = clamp(text, selection);
    let selected = &text[selection.clone()];

    // `**word**` selected, including the markers.
    if selected.len() >= marker.len() * 2
        && selected.starts_with(marker)
        && selected.ends_with(marker)
        && !(style == Inline::Italic && selected.starts_with("__"))
    {
        let inner = &selected[marker.len()..selected.len() - marker.len()];
        return Edit {
            range: selection.clone(),
            text: inner.to_string(),
            selection: selection.start..selection.start + inner.len(),
        };
    }

    // `word` selected with the markers just outside it.
    let before = selection.start.checked_sub(marker.len());
    let after = selection.end + marker.len();
    if let Some(before) = before
        && after <= text.len()
        && &text[before..selection.start] == marker
        && &text[selection.end..after] == marker
    {
        return Edit {
            range: before..after,
            text: selected.to_string(),
            selection: before..before + selected.len(),
        };
    }

    let target = if selection.is_empty() {
        word_at(text, selection.start)
    } else {
        trim_whitespace(text, selection)
    };

    if target.is_empty() {
        let placeholder = style.placeholder();
        let start = target.start + marker.len();
        return Edit {
            range: target,
            text: format!("{marker}{placeholder}{marker}"),
            selection: start..start + placeholder.len(),
        };
    }

    let inner = &text[target.clone()];
    let start = target.start + marker.len();
    Edit {
        text: format!("{marker}{inner}{marker}"),
        selection: start..start + inner.len(),
        range: target,
    }
}

/// Turn the selection into a link and select the URL placeholder.
pub fn insert_link(text: &str, selection: Range<usize>) -> Edit {
    const URL: &str = "https://";
    let selection = clamp(text, selection);
    let label = &text[selection.clone()];
    let label = if label.is_empty() { "link text" } else { label };
    let url_start = selection.start + label.len() + 3;
    Edit {
        text: format!("[{label}]({URL})"),
        selection: url_start..url_start + URL.len(),
        range: selection,
    }
}

/// Insert a fenced code block around the selection.
pub fn insert_code_block(text: &str, selection: Range<usize>) -> Edit {
    let selection = clamp(text, selection);
    let body = &text[selection.clone()];
    let at_line_start = selection.start == 0 || text[..selection.start].ends_with('\n');
    let lead = if at_line_start { "" } else { "\n" };
    let body_start = selection.start + lead.len() + 4;
    Edit {
        text: format!("{lead}```\n{body}\n```\n"),
        selection: body_start..body_start + body.len(),
        range: selection,
    }
}

/// Toggle a block style on every line the selection touches.
///
/// When every non-blank line already has the style it is removed; otherwise
/// it is applied, replacing any other heading, quote or list marker so the
/// command switches a line between block types in one step.
pub fn toggle_block(text: &str, selection: Range<usize>, style: Block) -> Edit {
    let selection = clamp(text, selection);
    let start = text[..selection.start].rfind('\n').map_or(0, |ix| ix + 1);
    let end = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        text[selection.end..]
            .find('\n')
            .map_or(text.len(), |ix| selection.end + ix)
    };

    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let has_style = |line: &str| {
        let (_, marker, _) = split_marker(line);
        marker_matches(marker, style)
    };
    let remove = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .all(|line| has_style(line))
        && lines.iter().any(|line| !line.trim().is_empty());

    let mut number = 0;
    let replaced: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return line.to_string();
            }
            let (indent, _, rest) = split_marker(line);
            if remove {
                return format!("{indent}{rest}");
            }
            number += 1;
            let prefix = match style {
                Block::Heading(level) => format!("{} ", "#".repeat(level.clamp(1, 6) as usize)),
                Block::Quote => "> ".to_string(),
                Block::BulletList => "- ".to_string(),
                Block::NumberedList => format!("{number}. "),
                Block::TaskList => "- [ ] ".to_string(),
            };
            format!("{indent}{prefix}{rest}")
        })
        .collect();

    let replacement = replaced.join("\n");
    let caret = if selection.is_empty() {
        // Keep the caret at the end of its line's content.
        start + replacement.len()
    } else {
        start
    };
    Edit {
        selection: if selection.is_empty() {
            caret..caret
        } else {
            start..start + replacement.len()
        },
        range: start..end,
        text: replacement,
    }
}

/// How a language comments out a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentStyle {
    /// A marker that runs to the end of the line, such as `//` or `#`.
    Line(&'static str),
    /// A marker pair around the line, such as `<!--` and `-->`.
    Wrap(&'static str, &'static str),
}

/// The comment style for a file with extension `ext` (lowercase), as VS Code
/// would toggle it; `None` for plain text, which has no comments.
pub fn comment_style(ext: Option<&str>) -> Option<CommentStyle> {
    use CommentStyle::*;
    Some(match ext? {
        "md" | "markdown" | "mdown" | "mkd" | "html" | "htm" | "xml" | "svg" | "vue" => {
            Wrap("<!--", "-->")
        }
        "css" | "scss" | "less" => Wrap("/*", "*/"),
        "rs" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" | "go" | "c" | "h"
        | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "zig" | "php" | "cs"
        | "scala" | "swift" | "proto" | "typ" | "jsonc" | "dart" => Line("//"),
        "py" | "rb" | "sh" | "bash" | "zsh" | "yml" | "yaml" | "toml" | "ex" | "exs" | "mk"
        | "mak" | "cmake" | "r" | "pl" | "conf" | "ini" | "nix" | "tf" | "dockerfile" => Line("#"),
        "lua" | "sql" | "hs" => Line("--"),
        "tex" | "sty" | "bib" => Line("%"),
        _ => return None,
    })
}

/// Comment out the lines the selection touches, or uncomment them when every
/// non-blank one already is (VS Code's Toggle Line Comment). The marker goes
/// at the block's smallest indent; blank lines are left alone; the whole
/// change is one edit, so one undo step.
pub fn toggle_line_comment(
    text: &str,
    selection: Range<usize>,
    style: CommentStyle,
) -> Option<Edit> {
    let len = text.len();
    let start = selection.start.min(len);
    let end = selection.end.min(len);
    let block_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    // A selection that ends where a line begins does not reach into it.
    let last_offset = if end > start && text[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let block_end = text[last_offset.max(block_start)..]
        .find('\n')
        .map_or(len, |at| last_offset.max(block_start) + at);
    let block = &text[block_start..block_end];
    let lines: Vec<&str> = block.split('\n').collect();
    let indent_of = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let filled: Vec<&&str> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if filled.is_empty() {
        return None;
    }
    let commented = |line: &str| {
        let body = line.trim_start_matches([' ', '\t']);
        match style {
            CommentStyle::Line(mark) => body.starts_with(mark),
            CommentStyle::Wrap(open, close) => {
                body.starts_with(open) && body.trim_end().ends_with(close)
            }
        }
    };
    let uncomment = filled.iter().all(|line| commented(line));
    let column = filled.iter().map(|line| indent_of(line)).min().unwrap_or(0);
    // Each line's new text, the column where it changed, and how many bytes
    // that inserted (+) or removed (-) before the body.
    let edited: Vec<(String, usize, isize)> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return (line.to_string(), 0, 0);
            }
            if uncomment {
                let at = indent_of(line);
                let (indent, body) = line.split_at(at);
                let (mark, close) = match style {
                    CommentStyle::Line(mark) => (mark, ""),
                    CommentStyle::Wrap(open, close) => (open, close),
                };
                let mut rest = &body[mark.len()..];
                let mut removed = mark.len();
                if let Some(stripped) = rest.strip_prefix(' ') {
                    rest = stripped;
                    removed += 1;
                }
                if !close.is_empty() {
                    let inner = rest.trim_end();
                    let inner = &inner[..inner.len() - close.len()];
                    rest = inner.strip_suffix(' ').unwrap_or(inner);
                }
                (format!("{indent}{rest}"), at, -(removed as isize))
            } else {
                let (indent, body) = line.split_at(column.min(line.len()));
                let (text, added) = match style {
                    CommentStyle::Line(mark) => (format!("{indent}{mark} {body}"), mark.len() + 1),
                    CommentStyle::Wrap(open, close) => {
                        (format!("{indent}{open} {body} {close}"), open.len() + 1)
                    }
                };
                (text, column, added as isize)
            }
        })
        .collect();
    let new_lines: Vec<&str> = edited.iter().map(|(line, _, _)| line.as_str()).collect();
    let new_block = new_lines.join("\n");
    let new_end = block_start + new_block.len();
    let selection = if start == end {
        // The caret stays on the same character of its line.
        let line_ix = text[block_start..start].matches('\n').count();
        let old_line_start = block_start
            + lines[..line_ix]
                .iter()
                .map(|line| line.len() + 1)
                .sum::<usize>();
        let new_line_start = block_start
            + new_lines[..line_ix]
                .iter()
                .map(|line| line.len() + 1)
                .sum::<usize>();
        let (line, at, delta) = &edited[line_ix];
        let col = start - old_line_start;
        let col = if col >= *at {
            (col as isize + delta).max(*at as isize) as usize
        } else {
            col
        };
        let caret = new_line_start + col.min(line.len());
        caret..caret
    } else {
        block_start..new_end
    };
    Some(Edit {
        range: block_start..block_end,
        text: new_block,
        selection,
    })
}

/// Which way a block of lines moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMove {
    Up,
    Down,
}

/// Move the lines the selection touches one line up or down, the way VS
/// Code's Move Line Up / Down (Alt+↑ / Alt+↓, Option on macOS) does, and
/// leave the selection on them.
///
/// `None` when there is nowhere to go — the first line up, the last one
/// down — so the caller can leave the document alone.
pub fn move_lines(text: &str, selection: Range<usize>, direction: LineMove) -> Option<Edit> {
    let len = text.len();
    let start = selection.start.min(len);
    let end = selection.end.min(len);

    // `split` keeps the document rebuildable (`parts.join("\n") == text`). A
    // trailing empty part means the text ends in a newline; that is where the
    // file stops, not a line of its own to move.
    let mut parts: Vec<&str> = text.split('\n').collect();
    let phantom = usize::from(parts.len() > 1 && parts.last() == Some(&""));
    let lines = parts.len() - phantom;
    if lines == 0 {
        return None;
    }

    let mut starts = Vec::with_capacity(lines);
    let mut offset = 0;
    for part in &parts[..lines] {
        starts.push(offset);
        offset += part.len() + 1;
    }
    let line_at = |offset: usize| {
        starts
            .partition_point(|line_start| *line_start <= offset)
            .saturating_sub(1)
    };

    let first = line_at(start);
    // A selection that ends where a line begins does not reach into it.
    let last = if end > start { line_at(end - 1) } else { first };

    let moved = match direction {
        LineMove::Up if first > 0 => {
            let line = parts.remove(first - 1);
            parts.insert(last, line);
            first - 1
        }
        LineMove::Down if last + 1 < lines => {
            let line = parts.remove(last + 1);
            parts.insert(first, line);
            first + 1
        }
        _ => return None,
    };

    // Where the block lands, so the selection travels with it.
    let block_start: usize = parts[..moved].iter().map(|part| part.len() + 1).sum();
    let shift = |offset: usize| offset + block_start - starts[first];

    Some(Edit {
        range: 0..len,
        text: parts.join("\n"),
        selection: shift(start)..shift(end),
    })
}

/// Split a line into its indentation, its block marker and its content.
fn split_marker(line: &str) -> (&str, &str, &str) {
    let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    let (indent, body) = line.split_at(indent_len);

    let marker_len = if let Some(rest) = body.strip_prefix('#') {
        let hashes = 1 + rest.len() - rest.trim_start_matches('#').len();
        let after = &body[hashes..];
        if hashes <= 6 && (after.is_empty() || after.starts_with(' ')) {
            hashes + usize::from(after.starts_with(' '))
        } else {
            0
        }
    } else if body.starts_with("> ") {
        2
    } else if body.starts_with('>') {
        1
    } else if body.starts_with("- [ ] ") || body.starts_with("- [x] ") || body.starts_with("- [X] ")
    {
        6
    } else if body.starts_with("- ") || body.starts_with("* ") || body.starts_with("+ ") {
        2
    } else {
        let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && body[digits..].starts_with(". ") {
            digits + 2
        } else {
            0
        }
    };

    (indent, &body[..marker_len], &body[marker_len..])
}

fn marker_matches(marker: &str, style: Block) -> bool {
    let marker = marker.trim_end();
    match style {
        Block::Heading(level) => marker == "#".repeat(level.clamp(1, 6) as usize),
        Block::Quote => marker == ">",
        Block::BulletList => matches!(marker, "-" | "*" | "+"),
        Block::NumberedList => {
            marker.ends_with('.')
                && marker[..marker.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit())
        }
        Block::TaskList => marker.starts_with("- ["),
    }
}

/// `range` without leading and trailing whitespace: CommonMark does not
/// treat `** bold **` as emphasis, so the markers must hug the text.
fn trim_whitespace(text: &str, range: Range<usize>) -> Range<usize> {
    let selected = &text[range.clone()];
    let start = range.start + (selected.len() - selected.trim_start().len());
    let end = range.end - (selected.len() - selected.trim_end().len());
    if start >= end {
        range.start..range.start
    } else {
        start..end
    }
}

/// The word around `offset`, or an empty range at `offset`.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(ix, _)| ix);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(ix, _)| offset + ix);
    start..end
}

fn clamp(text: &str, range: Range<usize>) -> Range<usize> {
    let floor = |mut ix: usize| {
        ix = ix.min(text.len());
        while !text.is_char_boundary(ix) {
            ix -= 1;
        }
        ix
    };
    let start = floor(range.start.min(range.end));
    let end = floor(range.start.max(range.end));
    start..end
}

#[cfg(test)]
mod tests {
    #[test]
    fn toggles_line_comments_at_the_smallest_indent() {
        use super::{CommentStyle::*, toggle_line_comment};
        let apply = |text: &str, sel: std::ops::Range<usize>, style| {
            let edit = toggle_line_comment(text, sel, style).unwrap();
            let mut out = text.to_string();
            out.replace_range(edit.range.clone(), &edit.text);
            (out, edit.selection)
        };
        // Multi-line, nested indent, blank line untouched; then back.
        let text = "fn f() {\n    a();\n\n        b();\n}\n";
        let sel = 9..text.find("b();").unwrap();
        let (on, sel_on) = apply(text, sel, Line("//"));
        assert_eq!(on, "fn f() {\n    // a();\n\n    //     b();\n}\n");
        let (off, _) = apply(&on, sel_on, Line("//"));
        assert_eq!(off, text);
        // Mixed: one line commented, one not -> comment both.
        let (mixed, _) = apply("# a\nb\n", 0..5, Line("#"));
        assert_eq!(mixed, "# # a\n# b\n");
        // A selection ending at column 0 leaves the next line alone.
        let (col0, _) = apply("a\nb\nc\n", 0..4, Line("#"));
        assert_eq!(col0, "# a\n# b\nc\n");
        // Markdown wraps, and unwraps.
        let (md, sel) = apply("text\n", 2..2, Wrap("<!--", "-->"));
        assert_eq!(md, "<!-- text -->\n");
        assert_eq!(sel, 7..7, "caret stays on the same character");
        let (back, _) = apply(&md, 0..0, Wrap("<!--", "-->"));
        assert_eq!(back, "text\n");
        // Blank-only selections do nothing.
        assert!(toggle_line_comment("\n\n", 0..1, Line("#")).is_none());
        assert_eq!(super::comment_style(Some("txt")), None);
        assert_eq!(super::comment_style(Some("typ")), Some(Line("//")));
        assert_eq!(super::comment_style(Some("py")), Some(Line("#")));
    }

    use super::*;

    fn apply(text: &str, edit: &Edit) -> String {
        let mut out = text.to_string();
        out.replace_range(edit.range.clone(), &edit.text);
        out
    }

    #[test]
    fn wraps_and_unwraps_selection() {
        let text = "make this bold";
        let edit = toggle_inline(text, 5..9, Inline::Bold);
        let bold = apply(text, &edit);
        assert_eq!(bold, "make **this** bold");
        assert_eq!(&bold[edit.selection.clone()], "this");

        // Toggling the same selection removes the markers again.
        let edit = toggle_inline(&bold, edit.selection, Inline::Bold);
        let plain = apply(&bold, &edit);
        assert_eq!(plain, text);
        assert_eq!(&plain[edit.selection], "this");

        // Selecting the markers too also unwraps.
        let edit = toggle_inline("**x**", 0..5, Inline::Bold);
        assert_eq!(apply("**x**", &edit), "x");
    }

    #[test]
    fn keeps_selected_whitespace_outside_the_markers() {
        let text = "## Section 7 ";
        let edit = toggle_inline(text, 3..13, Inline::Bold);
        assert_eq!(apply(text, &edit), "## **Section 7** ");
    }

    #[test]
    fn styles_word_under_caret_or_inserts_placeholder() {
        let edit = toggle_inline("say hello now", 6..6, Inline::Code);
        assert_eq!(apply("say hello now", &edit), "say `hello` now");

        let edit = toggle_inline("a  b", 2..2, Inline::Italic);
        let out = apply("a  b", &edit);
        assert_eq!(out, "a _italic text_ b");
        assert_eq!(&out[edit.selection], "italic text");
    }

    #[test]
    fn inserts_link_with_url_selected() {
        let edit = insert_link("see docs", 4..8);
        let out = apply("see docs", &edit);
        assert_eq!(out, "see [docs](https://)");
        assert_eq!(&out[edit.selection], "https://");
    }

    #[test]
    fn toggles_headings_and_switches_block_types() {
        let text = "Title\nbody";
        let edit = toggle_block(text, 2..2, Block::Heading(2));
        let heading = apply(text, &edit);
        assert_eq!(heading, "## Title\nbody");

        let edit = toggle_block(&heading, 3..3, Block::Heading(2));
        assert_eq!(apply(&heading, &edit), text);

        // A different level replaces the marker instead of stacking.
        let edit = toggle_block(&heading, 3..3, Block::Heading(1));
        assert_eq!(apply(&heading, &edit), "# Title\nbody");

        let edit = toggle_block("- item", 0..0, Block::Quote);
        assert_eq!(apply("- item", &edit), "> item");
    }

    #[test]
    fn numbers_and_toggles_list_lines() {
        let text = "one\n\ntwo\nthree";
        let edit = toggle_block(text, 0..text.len(), Block::NumberedList);
        let list = apply(text, &edit);
        assert_eq!(list, "1. one\n\n2. two\n3. three");

        let edit = toggle_block(&list, 0..list.len(), Block::NumberedList);
        assert_eq!(apply(&list, &edit), text);

        let edit = toggle_block("  todo", 0..0, Block::TaskList);
        assert_eq!(apply("  todo", &edit), "  - [ ] todo");
    }

    #[test]
    fn wraps_code_block_on_its_own_lines() {
        let edit = insert_code_block("x = 1", 0..5);
        let out = apply("x = 1", &edit);
        assert_eq!(out, "```\nx = 1\n```\n");
        assert_eq!(&out[edit.selection], "x = 1");
    }

    #[test]
    fn clamps_to_char_boundaries() {
        let text = "é";
        let edit = toggle_inline(text, 1..1, Inline::Bold);
        assert_eq!(apply(text, &edit), "**é**");
    }

    #[test]
    fn moves_the_line_the_caret_is_on_up_and_down() {
        let text = "one\ntwo\nthree\n";
        let down = move_lines(text, 4..4, LineMove::Down).expect("room below");
        assert_eq!(apply(text, &down), "one\nthree\ntwo\n");
        assert_eq!(down.selection, 10..10);

        let up = move_lines(text, 4..4, LineMove::Up).expect("room above");
        assert_eq!(apply(text, &up), "two\none\nthree\n");
        assert_eq!(up.selection, 0..0);
    }

    #[test]
    fn a_selection_travels_with_the_block_it_moves() {
        let text = "a\nb\nc\nd";
        let edit = move_lines(text, 2..5, LineMove::Down).expect("room below");
        let moved = apply(text, &edit);
        assert_eq!(moved, "a\nd\nb\nc");
        assert_eq!(&moved[edit.selection.clone()], "b\nc");
    }

    #[test]
    fn there_is_nowhere_to_go_at_the_ends() {
        let text = "a\nb\nc\n";
        assert!(move_lines(text, 0..0, LineMove::Up).is_none());
        assert!(move_lines(text, 4..4, LineMove::Down).is_none());
        assert!(move_lines("", 0..0, LineMove::Up).is_none());
        assert!(move_lines("", 0..0, LineMove::Down).is_none());
        // A caret after the trailing newline is on the file's last line, not
        // on a line below it.
        assert!(move_lines(text, 6..6, LineMove::Down).is_none());
    }

    #[test]
    fn the_trailing_newline_survives_a_move_and_is_not_a_line() {
        let text = "a\nb\n";
        let edit = move_lines(text, 0..0, LineMove::Down).expect("room below");
        assert_eq!(apply(text, &edit), "b\na\n");
        assert_eq!(edit.selection, 2..2);

        // Without a trailing newline the two lines still swap cleanly.
        let edit = move_lines("a\nb", 2..2, LineMove::Up).expect("room above");
        assert_eq!(apply("a\nb", &edit), "b\na");
        assert_eq!(edit.selection, 0..0);
    }
}
