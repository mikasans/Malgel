//! LaTeX math rendering, entirely in Rust.
//!
//! A formula is translated from LaTeX to Typst math markup, laid out by the
//! Typst compiler with its bundled New Computer Modern Math font, and
//! exported as SVG. Nothing here touches the file system or the network: the
//! compiler runs in a sealed world that only knows the formula's own source.

use typst::layout::{Abs, Frame, FrameItem};
use typst_layout::Page;

use crate::typst_world::{self, VirtualFiles};

/// A rendered formula. Sizes are in the pixels the formula was laid out for.
#[derive(Debug, Clone)]
pub struct MathSvg {
    pub svg: String,
    pub width: f32,
    pub height: f32,
    /// Distance from the top to the text baseline, for aligning inline math.
    pub baseline: f32,
}

/// How a formula sits in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MathStyle {
    /// Inside a line of text (`$…$`).
    Inline,
    /// On its own, centered and in display style (`$$…$$`).
    Display,
}

/// A formula rasterized as PNG, for formats that can't embed SVG.
pub struct MathPng {
    pub png: Vec<u8>,
    /// Size and baseline in points, as laid out (not in image pixels).
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
}

/// Render the LaTeX formula `tex` at `font_size` pixels in `color`
/// (`#rrggbbaa`). Returns a message suitable for the user on failure.
pub fn render(tex: &str, style: MathStyle, font_size: f32, color: &str) -> Result<MathSvg, String> {
    // One point is one logical pixel on screen: a 1pt frame never thins
    // out to a half-pixel smear.
    let page = layout(tex, style, font_size, color, 1.)?;
    let size = page.frame.size();
    Ok(MathSvg {
        svg: typst_svg::svg(&page, &typst_svg::SvgOptions::default()),
        width: size.x.to_pt() as f32,
        height: size.y.to_pt() as f32,
        baseline: baseline(&page.frame).to_pt() as f32,
    })
}

/// Render `tex` as a PNG with `pixels_per_point` pixels for every point of
/// its laid-out size, so it stays sharp when printed.
pub fn render_png(
    tex: &str,
    style: MathStyle,
    font_size: f32,
    color: &str,
    pixels_per_point: f32,
) -> Result<MathPng, String> {
    let stroke = (1. / pixels_per_point).max(0.5);
    let page = layout(tex, style, font_size, color, stroke)?;
    let size = page.frame.size();
    let png = typst_world::rasterize(&page, pixels_per_point)?;
    Ok(MathPng {
        png,
        width: size.x.to_pt() as f32,
        height: size.y.to_pt() as f32,
        baseline: baseline(&page.frame).to_pt() as f32,
    })
}

/// Marks a formula's source as Typst math rather than LaTeX. It rides in
/// the source string, so caches, exports and error displays carry it
/// without a second code path; [`shown_source`] drops it for display.
const TYPST_MARK: &str = "\u{E000}typst\u{E000}";

/// The fence languages that hold native Typst math: ```typst-math (and
/// the short ```typ-math), drawn like a `$$` block.
pub fn is_typst_math_fence(lang: Option<&str>) -> bool {
    lang.map(str::trim).is_some_and(|lang| {
        lang.eq_ignore_ascii_case("typst-math") || lang.eq_ignore_ascii_case("typ-math")
    })
}

/// A formula source for Typst math `body`, for [`render`] and friends.
pub fn typst_source(body: &str) -> String {
    format!("{TYPST_MARK}{body}")
}

/// The formula as its author wrote it, for display and alt text.
pub fn shown_source(tex: &str) -> &str {
    tex.strip_prefix(TYPST_MARK).unwrap_or(tex).trim()
}

/// Translate the LaTeX formula `tex` to Typst math markup (without the
/// surrounding dollars).
pub fn to_typst(tex: &str) -> Result<String, String> {
    // Typst math written as such (a ```typst-math fence) needs no converter.
    if let Some(math) = tex.strip_prefix(TYPST_MARK) {
        return Ok(math.trim().to_string());
    }
    let tex = tex.trim();
    // The converter closes whatever is left open; say so instead of
    // quietly drawing something else.
    if let Some(problem) = unbalanced(tex) {
        return Err(format!("Couldn’t read the formula: {problem}."));
    }
    // The converter is written for well-formed input; formulas are converted
    // as they are typed, so never let a half-written one take down a thread.
    let fenced = paired_bars(tex);
    let spaced = space_after_groups(&fenced);
    let math = std::panic::catch_unwind(|| tylax::latex_to_typst(&spaced))
        .map_err(|_| "Couldn’t read the formula.".to_string())?;
    // It reports what it can't match as a comment and carries on.
    if math.contains("/* LaTeX Error") {
        return Err("Couldn’t read the formula: a brace or environment isn’t closed.".into());
    }
    let math = math.trim();
    // Environments such as `align` come back as a whole equation.
    let math = math
        .strip_prefix('$')
        .and_then(|math| math.strip_suffix('$'))
        .unwrap_or(math)
        .trim();
    // `\boxed` comes back with a 2pt inset, which lets tall glyphs touch
    // the frame; LaTeX's \fboxsep is 3pt.
    let math = space_cases(math).replace(
        "#box(stroke: 0.5pt, inset: 2pt,",
        "#box(stroke: 0.5pt, inset: 3pt,",
    );
    Ok(text_strings(&math))
}

/// Balanced `|…|`, `\lvert…\rvert`, `\|…\|` and `\lVert…\rVert` become
/// `\left|…\right|` (and `\left\|…\right\|`), which the converter turns into
/// Typst's `abs(…)` / `norm(…)`. A bare `|` is a plain symbol in Typst, so
/// `\min_t |\beta(t)|` set the first bar against `min`. Bars pair up in
/// order within one group (braces, `\left…\right`); a group with an odd
/// number of them (set-builder `|`) or sized ones (`\big|`) is left alone.
fn paired_bars(tex: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Bar {
        Single,
        Double,
        Open(bool),
        Close(bool),
    }
    // (byte range, kind) per group, groups on a stack.
    let mut stack: Vec<Vec<(std::ops::Range<usize>, Bar)>> = vec![Vec::new()];
    let mut edits: Vec<(std::ops::Range<usize>, &'static str)> = Vec::new();
    let mut sized = vec![false];
    let close = |group: Vec<(std::ops::Range<usize>, Bar)>,
                 sized: bool,
                 edits: &mut Vec<(std::ops::Range<usize>, &'static str)>| {
        if sized {
            return;
        }
        for (kind, open, shut) in [
            (Bar::Single, r"\left|", r"\right|"),
            (Bar::Double, r"\left\|", r"\right\|"),
        ] {
            let bars: Vec<_> = group.iter().filter(|(_, k)| *k == kind).collect();
            if bars.len() % 2 == 0 {
                for (n, (range, _)) in bars.into_iter().enumerate() {
                    edits.push((range.clone(), if n % 2 == 0 { open } else { shut }));
                }
            }
        }
        for double in [false, true] {
            let (open, shut) = if double {
                (r"\left\|", r"\right\|")
            } else {
                (r"\left|", r"\right|")
            };
            let mut pending: Vec<std::ops::Range<usize>> = Vec::new();
            let mut pairs = Vec::new();
            let mut ok = true;
            for (range, kind) in &group {
                match kind {
                    Bar::Open(d) if *d == double => pending.push(range.clone()),
                    Bar::Close(d) if *d == double => match pending.pop() {
                        Some(start) => pairs.push((start, range.clone())),
                        None => ok = false,
                    },
                    _ => {}
                }
            }
            if ok && pending.is_empty() {
                for (a, b) in pairs {
                    edits.push((a, open));
                    edits.push((b, shut));
                }
            }
        }
    };
    let bytes = tex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let start = i;
                i += 1;
                let name_end = bytes[i..]
                    .iter()
                    .position(|b| !b.is_ascii_alphabetic())
                    .map_or(bytes.len(), |n| i + n);
                if name_end == i {
                    // `\|`, `\{`, `\\`…: one escaped char.
                    if i < bytes.len() && bytes[i] == b'|' {
                        stack.last_mut().unwrap().push((start..i + 1, Bar::Double));
                    }
                    i += tex[i..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                let name = &tex[i..name_end];
                i = name_end;
                match name {
                    "lvert" => stack.last_mut().unwrap().push((start..i, Bar::Open(false))),
                    "rvert" => stack
                        .last_mut()
                        .unwrap()
                        .push((start..i, Bar::Close(false))),
                    "lVert" => stack.last_mut().unwrap().push((start..i, Bar::Open(true))),
                    "rVert" => stack.last_mut().unwrap().push((start..i, Bar::Close(true))),
                    "left" | "right" | "middle" | "big" | "Big" | "bigg" | "Bigg" | "bigl"
                    | "bigr" | "Bigl" | "Bigr" | "biggl" | "biggr" | "Biggl" | "Biggr" => {
                        // Skip the delimiter that follows, whatever it is.
                        while i < bytes.len() && bytes[i] == b' ' {
                            i += 1;
                        }
                        let delim_bar = bytes.get(i) == Some(&b'|')
                            || (bytes.get(i) == Some(&b'\\') && bytes.get(i + 1) == Some(&b'|'));
                        if delim_bar && !matches!(name, "left" | "right") {
                            *sized.last_mut().unwrap() = true;
                        }
                        if bytes.get(i) == Some(&b'\\') {
                            i += 1;
                            let n = bytes[i..]
                                .iter()
                                .position(|b| !b.is_ascii_alphabetic())
                                .unwrap_or(bytes.len() - i);
                            i += n.max(1).min(bytes.len() - i);
                        } else if i < bytes.len() {
                            i += tex[i..].chars().next().map_or(1, char::len_utf8);
                        }
                        if name == "left" {
                            stack.push(Vec::new());
                            sized.push(false);
                        } else if name == "right" && stack.len() > 1 {
                            let group = stack.pop().unwrap();
                            let was = sized.pop().unwrap();
                            close(group, was, &mut edits);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            b'{' => {
                stack.push(Vec::new());
                sized.push(false);
            }
            b'}' if stack.len() > 1 => {
                let group = stack.pop().unwrap();
                let was = sized.pop().unwrap();
                close(group, was, &mut edits);
            }
            b'|' => stack.last_mut().unwrap().push((i..i + 1, Bar::Single)),
            _ => {}
        }
        i += 1;
    }
    // Unclosed groups (a half-typed formula): leave their bars alone.
    let outer = stack.swap_remove(0);
    if stack.is_empty() {
        close(outer, sized[0], &mut edits);
    }
    if edits.is_empty() {
        return tex.to_string();
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(tex.len() + edits.len() * 6);
    let mut at = 0;
    for (range, text) in edits {
        out.push_str(&tex[at..range.start]);
        out.push_str(text);
        // `\left|x` must not glue to a following letter: `\right|` before
        // `x` is fine, but `\left\|` + `v` reads as a command otherwise.
        out.push(' ');
        at = range.end;
    }
    out.push_str(&tex[at..]);
    out
}

/// A space between a closing brace and the command after it: the converter
/// glues them, so `\frac{1}{T}\sum` came out as `1/Tsum` (an unknown
/// variable). LaTeX ignores the space.
fn space_after_groups(tex: &str) -> String {
    let mut out = String::with_capacity(tex.len() + 8);
    let mut chars = tex.chars().peekable();
    while let Some(ch) = chars.next() {
        out.push(ch);
        if ch == '}' && chars.peek() == Some(&'\\') {
            out.push(' ');
        }
    }
    out
}

/// `\text{…}` arrives as `#text[…]` content; Typst's own form for text in
/// math is a string, `"…"`, which takes any script (CJK, full-width
/// punctuation) as is and is never read as markup. Content that holds
/// markup (math, escapes, brackets) stays as it is.
fn text_strings(math: &str) -> String {
    const OPEN: &str = "#text[";
    let mut out = String::with_capacity(math.len());
    let mut rest = math;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let body_start = start + OPEN.len();
        match rest[body_start..].find(']') {
            Some(len)
                if !rest[body_start..body_start + len].contains(['$', '#', '[', '\\', '"']) =>
            {
                out.push('"');
                out.push_str(&rest[body_start..body_start + len]);
                out.push('"');
                rest = &rest[body_start + len + 1..];
            }
            _ => {
                out.push_str(OPEN);
                rest = &rest[body_start..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether `text` has Chinese, Japanese or Korean characters (or the
/// full-width forms that go with them), which the bundled fonts lack.
fn needs_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(ch as u32,
            0x1100..=0x11FF
            | 0x2E80..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF)
    })
}

/// Put a quad between each case and its condition, as LaTeX's `cases` does;
/// Typst's aligns them with no gap.
fn space_cases(math: &str) -> String {
    const OPEN: &str = "cases(";
    let mut out = String::with_capacity(math.len());
    let mut rest = math;
    while let Some(start) = rest.find(OPEN) {
        let (before, after) = rest.split_at(start + OPEN.len());
        out.push_str(before);
        // Walk the arguments, spacing alignment points at their own level.
        let mut depth = 0usize;
        let mut in_string = false;
        let mut end = after.len();
        let mut chars = after.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '"' => in_string = !in_string,
                '\\' if in_string => {
                    chars.next();
                }
                _ if in_string => {}
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => {
                    end = index;
                    break;
                }
                ')' | ']' | '}' => depth -= 1,
                '&' if depth == 0 => {
                    out.push_str("& quad");
                    continue;
                }
                _ => {}
            }
            out.push(ch);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// What is left open (or closed without being opened) in `tex`, if anything.
fn unbalanced(tex: &str) -> Option<&'static str> {
    let (mut braces, mut environments, mut delimiters) = (0i32, 0i32, 0i32);
    let mut chars = tex.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let mut name = String::new();
                while let Some(&next) = chars.peek() {
                    if !next.is_ascii_alphabetic() {
                        break;
                    }
                    name.push(next);
                    chars.next();
                }
                match name.as_str() {
                    // An escaped character such as `\{` or `\\`.
                    "" => {
                        chars.next();
                    }
                    "begin" => environments += 1,
                    "end" => environments -= 1,
                    "left" => delimiters += 1,
                    "right" => delimiters -= 1,
                    _ => {}
                }
            }
            '{' => braces += 1,
            '}' => braces -= 1,
            _ => {}
        }
        if braces < 0 {
            return Some("a closing brace has no opening one");
        }
        if environments < 0 {
            return Some("an \\end has no \\begin");
        }
        if delimiters < 0 {
            return Some("a \\right has no \\left");
        }
    }
    if braces > 0 {
        Some("a brace isn’t closed")
    } else if environments > 0 {
        Some("a \\begin has no \\end")
    } else if delimiters > 0 {
        Some("a \\left has no \\right")
    } else {
        None
    }
}

/// Typst markup for the Typst math `math` as an equation in `style`.
pub fn equation(math: &str, style: MathStyle) -> String {
    match style {
        // Spaces inside the dollars make Typst lay out a display equation.
        MathStyle::Display => format!("$ {math} $"),
        // A box keeps the formula's baseline in the page frame, which
        // Typst otherwise flattens away for single-term formulas.
        MathStyle::Inline => format!("#box[${math}$]"),
    }
}

/// The Typst math for `tex` once it is known to typeset, for documents
/// where one bad formula would otherwise stop the whole document.
pub fn checked_typst(tex: &str, style: MathStyle) -> Result<String, String> {
    let math = to_typst(tex)?;
    typeset(&equation(&math, style), 11., "#000000ff")?;
    Ok(math)
}

fn layout(
    tex: &str,
    style: MathStyle,
    font_size: f32,
    color: &str,
    stroke_pt: f32,
) -> Result<Page, String> {
    // Formulas are laid out as they are typed: a compiler panic on one
    // costs that formula, never the preview thread.
    crate::crash::isolate("math layout", || {
        let math = framed_in(&to_typst(tex)?, color, stroke_pt);
        typeset(&equation(&math, style), font_size, color)
    })
    .unwrap_or_else(|| Err("Couldn’t lay out the formula.".into()))
}

/// `\boxed` frames drawn in the formula's own colour, `stroke_pt` wide:
/// the converter's bare `stroke: 0.5pt` is black, which vanishes on a dark
/// theme and is a faint half-pixel on a light one. Only on screen and in
/// HTML; the PDF keeps LaTeX's thin black rule.
fn framed_in(math: &str, color: &str, stroke_pt: f32) -> String {
    // A stroke straddles the box's edge, and the page ends at that edge: a
    // negative outset of half the width keeps all of it on the page.
    let half = stroke_pt / 2.;
    math.replace(
        "#box(stroke: 0.5pt,",
        &format!("#box(stroke: {stroke_pt}pt + rgb(\"{color}\"), outset: -{half}pt,"),
    )
}

fn typeset(body: &str, font_size: f32, color: &str) -> Result<Page, String> {
    // Text in CJK scripts needs fonts Typst doesn't bundle: compile those
    // formulas with the system's CJK faces too, named after the default
    // Latin font so Latin text keeps its look. Other formulas never load
    // them.
    let (fonts, font_list, math_fonts) = if needs_cjk(body) {
        let fonts = &*typst_world::CJK_MATH_FONTS;
        let families = typst_world::cjk_families(fonts);
        if families.is_empty() {
            return Err(
                "This formula has Chinese, Japanese or Korean text, but no CJK font \
                 is installed (for example Noto Serif CJK SC)."
                    .into(),
            );
        }
        let quoted = |first: &'static str| {
            std::iter::once(first)
                .chain(families.iter().copied())
                .map(|family| format!("\"{family}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        // Text in an equation (`"中文"`) is set in the math font: name the
        // CJK families after it too, or Typst picks any face that has the
        // glyphs.
        (
            fonts,
            format!(", font: ({})", quoted("libertinus serif")),
            format!(
                "#show math.equation: set text(font: ({}))",
                quoted("new computer modern math")
            ),
        )
    } else {
        (&*typst_world::BUNDLED_FONTS, String::new(), String::new())
    };
    const PREAMBLE_LINES: usize = 4;
    let source = format!(
        "#set page(width: auto, height: auto, margin: 0pt, fill: none)\n\
         #set text(size: {font_size}pt, fill: rgb(\"{color}\"), top-edge: \"bounds\", bottom-edge: \"bounds\"{font_list})\n\
         #set math.equation(numbering: none)\n\
         {math_fonts}\n\
         {body}\n"
    );

    let document = typst_world::compile_located(source, VirtualFiles::new(), fonts).map_err(
        |(error, line)| match line.and_then(|line| line.checked_sub(PREAMBLE_LINES)) {
            Some(line) if line > 0 => {
                let text = body.lines().nth(line - 1).unwrap_or("").trim();
                format!("Couldn’t typeset the formula: {error} (Typst line {line}: {text}).")
            }
            _ => format!("Couldn’t typeset the formula: {error}."),
        },
    )?;
    document
        .pages()
        .first()
        .cloned()
        .ok_or_else(|| "The formula is empty.".to_string())
}

/// The baseline of the first line of `frame`: the page holds a paragraph
/// whose line frames carry their baseline.
fn baseline(frame: &Frame) -> Abs {
    for (position, item) in frame.items() {
        if let FrameItem::Group(group) = item {
            if group.frame.has_baseline() {
                return position.y + group.frame.baseline();
            }
            let inner = baseline(&group.frame);
            if inner < group.frame.height() {
                return position.y + inner;
            }
        }
    }
    frame.baseline()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_inline_and_display_math() {
        let inline = render("x^2 + y_1", MathStyle::Inline, 16., "#000000ff").unwrap();
        assert!(inline.svg.starts_with("<svg"));
        assert!(inline.width > 10. && inline.height > 5.);
        assert!(inline.baseline > 0. && inline.baseline < inline.height);

        // Single terms too: the subscript hangs below the baseline.
        let term = render("y_1", MathStyle::Inline, 16., "#000000ff").unwrap();
        assert!(term.baseline < term.height * 0.8);

        let display = render(
            r"\sum_{i=1}^{n} \frac{1}{i^2}",
            MathStyle::Display,
            16.,
            "#000000ff",
        )
        .unwrap();
        // Display style stacks the limits and the fraction.
        assert!(display.height > inline.height * 1.5);
    }

    #[test]
    fn rasterizes_for_documents() {
        let png = render_png(r"\frac{a}{b}", MathStyle::Display, 16., "#000000ff", 4.).unwrap();
        assert!(png.png.starts_with(b"\x89PNG"));
        assert!(png.height > png.width * 0.5);
    }

    #[test]
    fn scales_with_font_size() {
        let small = render(r"\alpha", MathStyle::Inline, 12., "#000000ff").unwrap();
        let large = render(r"\alpha", MathStyle::Inline, 24., "#000000ff").unwrap();
        assert!((large.width / small.width - 2.).abs() < 0.1);
    }

    #[test]
    fn reports_unsupported_input() {
        for tex in [
            r"\frac{1}{",
            r"\frac{a}",
            "}",
            r"\end{cases}",
            r"\foo{x}",
            r"\sqrt",
            r"x^{2",
            r"\left( x",
            r"\begin{matrix} a",
        ] {
            assert!(
                render(tex, MathStyle::Inline, 16., "#000000ff").is_err(),
                "{tex}"
            );
            assert!(checked_typst(tex, MathStyle::Inline).is_err(), "{tex}");
        }
    }

    #[test]
    fn finds_what_is_left_open() {
        assert_eq!(unbalanced(r"\frac{1}{"), Some("a brace isn’t closed"));
        assert_eq!(unbalanced("a}"), Some("a closing brace has no opening one"));
        assert_eq!(
            unbalanced(r"\begin{cases} x"),
            Some("a \\begin has no \\end")
        );
        assert_eq!(unbalanced(r"\left( x"), Some("a \\left has no \\right"));
        // Escaped braces and line breaks aren't groups.
        assert_eq!(unbalanced(r"\{ a \} \\ {b}"), None);
        assert_eq!(unbalanced(r"\left\{ x \right."), None);
        assert_eq!(unbalanced(r"\begin{cases} 1 \end{cases}"), None);
    }

    #[test]
    fn spaces_cases_like_latex() {
        assert_eq!(
            space_cases(r#"f(x) = cases(1 & x > 0, 0 & #text[otherwise])"#),
            r#"f(x) = cases(1 & quad x > 0, 0 & quad #text[otherwise])"#
        );
        // Only the alignment points of the cases themselves.
        assert_eq!(
            space_cases(r#"cases(mat(a & b) & "&", x) & y"#),
            r#"cases(mat(a & b) & quad "&", x) & y"#
        );
    }

    #[test]
    fn translates_common_latex() {
        let cases = [
            (r"\frac{a}{b}", "a/b"),
            (r"\sqrt[3]{x}", "root(3, x)"),
            (r"\alpha \le \beta", "alpha <= beta"),
            (r"\mathbb{R}^n", "RR^(n)"),
            (
                r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
                r#"mat(delim: "(", a, b ; c, d)"#,
            ),
            (
                r"\begin{align} a &= b \\ c &= d \end{align}",
                r"a & = b \ c & = d",
            ),
        ];
        for (tex, typst) in cases {
            assert_eq!(to_typst(tex).unwrap(), typst, "{tex}");
        }
    }

    #[test]
    fn a_boxed_frame_is_drawn_in_the_formula_colour() {
        // Light text on a dark theme: the frame's edge pixels carry it.
        let png = render_png(r"\boxed{x+1}", MathStyle::Display, 16., "#e6e6e6ff", 2.).unwrap();
        let image = image::load_from_memory(&png.png).unwrap().to_rgba8();
        let (w, h) = image.dimensions();
        let lit = |x: u32, y: u32| {
            let p = image.get_pixel(x, y);
            p[3] > 200 && p[0] > 150 && p[1] > 150 && p[2] > 150
        };
        let row = h / 2;
        let left = (0..w / 4).any(|x| lit(x, row));
        let right = (w - w / 4..w).any(|x| lit(x, row));
        let top = (0..h / 4).any(|y| lit(w / 2, y));
        assert!(left && right && top, "frame edges missing ({w}x{h})");
        // The SVG carries the colour on the frame too (HTML export swaps it
        // for currentColor), at least one point wide.
        let svg = render(r"\boxed{x}", MathStyle::Display, 16., "#123456ff")
            .unwrap()
            .svg;
        assert!(svg.contains("#123456"), "{svg}");
        assert!(!svg.contains("stroke=\"#000000\""), "{svg}");
        assert_eq!(
            framed_in("#box(stroke: 0.5pt, inset: 3pt, $x$)", "#fff", 1.),
            "#box(stroke: 1pt + rgb(\"#fff\"), outset: -0.5pt, inset: 3pt, $x$)"
        );
    }

    #[test]
    fn typst_math_fences_typeset_natively() {
        assert!(is_typst_math_fence(Some("typst-math")));
        assert!(is_typst_math_fence(Some("typ-math")));
        assert!(!is_typst_math_fence(Some("typst")));
        assert!(!is_typst_math_fence(None));
        let source = typst_source("sum_(i=1)^n i = (n(n+1))/2 \"中文\"\n");
        assert_eq!(shown_source(&source), "sum_(i=1)^n i = (n(n+1))/2 \"中文\"");
        assert_eq!(
            to_typst(&source).unwrap(),
            "sum_(i=1)^n i = (n(n+1))/2 \"中文\""
        );
        assert!(render(&source, MathStyle::Display, 16., "#000000ff").is_ok());
        // Errors name the Typst line, as for LaTeX.
        let error = render(
            &typst_source("a\nb +\nunknownthing"),
            MathStyle::Display,
            16.,
            "#000000ff",
        )
        .unwrap_err();
        assert!(error.contains("unknownthing"), "{error}");
        assert!(error.contains("line 3"), "{error}");
    }

    #[test]
    fn balanced_bars_become_abs_and_norm() {
        for (tex, typst) in [
            (r"\max \min_t |\beta(t)|", "max min_(t) abs(beta (t))"),
            (r"|a| + |b|", "abs(a) + abs(b)"),
            (r"|x|^2", "abs(x)^(2)"),
            (r"\frac{|a|}{|b|}", "frac(abs(a), abs(b))"),
            (r"\lvert x \rvert", "abs(x)"),
            (r"\|v\|", "norm(v)"),
            (r"\| x \|_2", "norm(x)_(2)"),
            (r"\lVert x\rVert", "norm(x)"),
            (r"\left| x \right|", "abs(x)"),
            // A lone set-builder bar stays a bar; sized bars are untouched.
            (r"\{ x | x > 0 \}", "{ x | x > 0 }"),
        ] {
            assert_eq!(to_typst(tex).unwrap(), typst, "{tex}");
            assert!(checked_typst(tex, MathStyle::Display).is_ok(), "{tex}");
        }
        assert_eq!(paired_bars(r"\big| x \big|"), r"\big| x \big|");
        assert_eq!(paired_bars(r"\text{a|b} |c"), r"\text{a|b} |c");
    }

    #[test]
    fn typesets_the_usual_constructs() {
        for tex in [
            r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
            r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}",
            r"f(x) = \begin{cases} 1 & x > 0 \\ 0 & \text{otherwise} \end{cases}",
            r"\begin{aligned} x &= 1 \\ y &= 2 \end{aligned}",
            r"\begin{align} a &= b \\ c &= d \end{align}",
            r"\left( \frac{1}{2} \right) \langle x, y \rangle \| v \|",
            r"\mathbf{v} \cdot \hat{x} \vec{v} \overline{AB} \operatorname{sin} x",
            r"\lim_{x \to 0} \frac{\sin x}{x} \neq \infty",
            r"\binom{n}{k} \color{red}{x} \mathrm{d}x",
        ] {
            assert!(checked_typst(tex, MathStyle::Display).is_ok(), "{tex}");
        }
    }

    /// Glyph id 0 is a font's .notdef box ("tofu"): no glyph for the char.
    fn tofu(frame: &Frame) -> usize {
        frame
            .items()
            .map(|(_, item)| match item {
                FrameItem::Group(group) => tofu(&group.frame),
                FrameItem::Text(text) => text.glyphs.iter().filter(|g| g.id == 0).count(),
                _ => 0,
            })
            .sum()
    }

    fn typeset_tex(tex: &str, style: MathStyle) -> Result<Page, String> {
        typeset(&equation(&to_typst(tex)?, style), 16., "#000000ff")
    }

    /// The formulas of a Chinese research note: CJK in `\text`, full-width
    /// punctuation, `\boxed` around `gathered`, `\mathbb`, `\mathcal`,
    /// `\mathbf 1`, `\left[ \right]`, `\Downarrow`.
    const NOTE_FORMULAS: &[&str] = &[
        r"J_j=(a_j,d_j,w_j,D_j,\mathcal G_j(t))",
        r"\boxed{G_{\mathrm{FreeMig}}>G_{\mathrm{NoMig}}}",
        r"G(t)=(\mathcal S\cup\mathcal C,\mathcal E(t))",
        r"y_{rik}(t)\in\{0,1\}",
        r"\boxed{\max_{\pi}\; \mathbb E\left[ \frac{1}{T}\sum_j w_j\mathbf 1(C_j^\pi\le d_j) \right]}",
        r"\boxed{\begin{gathered}\text{动态推理负载 + 变化的卫星计算能力}\\ \Downarrow\\ \text{需要动态重分配（Claim 1）}\\ \Downarrow\\ \text{迁移收益与资源成本联合优化}\end{gathered}}",
        r"\text{中文}x",
    ];

    #[test]
    fn cjk_text_is_a_typst_string() {
        assert_eq!(to_typst(r"\text{中文}x").unwrap(), "\"中文\" x");
        let boxed = to_typst(NOTE_FORMULAS[5]).unwrap();
        assert!(
            boxed.starts_with("#box(stroke: 0.5pt, inset: 3pt,"),
            "{boxed}"
        );
        assert!(boxed.contains("\"需要动态重分配（Claim 1）\""), "{boxed}");
        assert!(boxed.contains("arrow.b.double"), "{boxed}");
        assert!(!boxed.contains("#text["), "{boxed}");
        // `\frac{1}{T}\sum` used to come out as the unknown `Tsum`.
        let max = to_typst(NOTE_FORMULAS[4]).unwrap();
        assert!(!max.contains("Tsum"), "{max}");
    }

    #[test]
    fn a_chinese_note_typesets_every_formula_without_tofu() {
        let cjk_installed = !typst_world::cjk_families(&typst_world::CJK_MATH_FONTS).is_empty();
        for tex in NOTE_FORMULAS {
            for style in [MathStyle::Display, MathStyle::Inline] {
                match typeset_tex(tex, style) {
                    Ok(page) => assert_eq!(tofu(&page.frame), 0, "tofu in {tex}"),
                    Err(error) => {
                        assert!(needs_cjk(tex) && !cjk_installed, "{tex}: {error}");
                        assert!(error.contains("no CJK font"), "{error}");
                    }
                }
            }
        }
        // The check can see tofu: the bundled fonts alone draw boxes.
        let source = equation(&to_typst(NOTE_FORMULAS[6]).unwrap(), MathStyle::Display);
        let bundled =
            typst_world::compile(source, VirtualFiles::new(), &typst_world::BUNDLED_FONTS).unwrap();
        assert!(tofu(&bundled.pages()[0].frame) > 0);
        // Latin-only formulas keep the bundled fonts alone.
        assert!(!needs_cjk(NOTE_FORMULAS[4]));
    }

    #[test]
    #[ignore = "timing probe"]
    fn cjk_formula_timing() {
        let t = std::time::Instant::now();
        let _ = &*typst_world::CJK_MATH_FONTS;
        let index = t.elapsed();
        let t = std::time::Instant::now();
        typeset_tex(NOTE_FORMULAS[5], MathStyle::Display).unwrap();
        let first = t.elapsed();
        let t = std::time::Instant::now();
        typeset_tex(NOTE_FORMULAS[5], MathStyle::Display).unwrap();
        let again = t.elapsed();
        let t = std::time::Instant::now();
        typeset_tex(NOTE_FORMULAS[0], MathStyle::Display).unwrap();
        let latin = t.elapsed();
        eprintln!("TIMING index {index:?} first {first:?} again {again:?} latin {latin:?}");
    }

    #[test]
    fn a_typst_error_names_its_line() {
        let error = typeset("$ a\nunknownthing $", 11., "#000000ff").unwrap_err();
        assert!(error.contains("Typst line 2"), "{error}");
        assert!(error.contains("unknownthing"), "{error}");
    }
}
