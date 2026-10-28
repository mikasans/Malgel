//! Document analysis that runs off the UI thread.
//!
//! One pass over the source produces everything the chrome needs besides the
//! rendered preview: word/character statistics for the status bar, the line
//! where every top-level block starts (so the preview can follow the editor),
//! the heading outline, and a content hash used to detect when an edit has
//! returned the document to its saved state.

use std::hash::{Hash as _, Hasher as _};

use markdown::{ParseOptions, mdast::Node};

/// Words per minute used for the reading-time estimate.
const READING_WORDS_PER_MINUTE: usize = 230;

/// Status bar statistics for a document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub words: usize,
    pub characters: usize,
    pub lines: usize,
}

impl Stats {
    /// Count words, characters and lines in `source`.
    ///
    /// A word is a run of letters, digits, apostrophes or connecting
    /// punctuation. Ideographic scripts do not separate words with spaces, so
    /// every CJK, Hiragana, Katakana or Hangul syllable counts as one word,
    /// which matches how those languages are usually measured.
    pub fn of(source: &str) -> Self {
        let mut counter = StatsCounter::default();
        counter.feed(source);
        counter.finish()
    }

    /// Estimated reading time in whole minutes; at least one for any text.
    pub fn reading_minutes(&self) -> usize {
        if self.words == 0 {
            0
        } else {
            self.words.div_ceil(READING_WORDS_PER_MINUTE)
        }
    }
}

/// Counts [`Stats`] over text fed in pieces.
#[derive(Default)]
struct StatsCounter {
    words: usize,
    characters: usize,
    newlines: usize,
    in_word: bool,
}

impl StatsCounter {
    fn feed(&mut self, text: &str) {
        for ch in text.chars() {
            self.characters += 1;
            if ch == '\n' {
                self.newlines += 1;
            }
            if is_ideograph(ch) {
                self.words += 1;
                self.in_word = false;
            } else if ch.is_alphanumeric() || (self.in_word && matches!(ch, '\'' | '’' | '_' | '-'))
            {
                if !self.in_word {
                    self.words += 1;
                    self.in_word = true;
                }
            } else {
                self.in_word = false;
            }
        }
    }

    /// Every line ends at a newline except the last, which may be empty.
    fn finish(&self) -> Stats {
        Stats {
            words: self.words,
            characters: self.characters,
            lines: self.newlines + 1,
        }
    }
}

fn is_ideograph(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F)
}

/// A heading in the document outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    /// Zero-based source line of the heading.
    pub line: usize,
}

/// Maps between editor lines and preview blocks.
///
/// The preview renders every top-level Markdown block as one row of a
/// virtual list, so the start line of each block is enough to translate a
/// scroll position in either direction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockIndex {
    /// Zero-based start line of every top-level block, ascending.
    starts: Vec<usize>,
    /// Total number of lines in the document.
    line_count: usize,
}

impl BlockIndex {
    pub fn new(starts: Vec<usize>, line_count: usize) -> Self {
        debug_assert!(starts.windows(2).all(|pair| pair[0] <= pair[1]));
        Self {
            starts,
            line_count: line_count.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// The block containing `line`, and how far through it `line` is (0..1).
    pub fn block_at_line(&self, line: f32) -> Option<(usize, f32)> {
        if self.starts.is_empty() {
            return None;
        }
        let ix = self
            .starts
            .partition_point(|start| *start as f32 <= line)
            .saturating_sub(1);
        let start = self.starts[ix] as f32;
        let end = self.block_end(ix) as f32;
        let fraction = if end > start {
            ((line - start) / (end - start)).clamp(0., 1.)
        } else {
            0.
        };
        Some((ix, fraction))
    }

    /// The source line `fraction` of the way through block `ix`.
    pub fn line_at_block(&self, ix: usize, fraction: f32) -> Option<f32> {
        let start = *self.starts.get(ix)? as f32;
        let end = self.block_end(ix) as f32;
        Some(start + (end - start) * fraction.clamp(0., 1.))
    }

    /// The first line after block `ix`: where the next block starts, or the
    /// end of the document for the last block.
    fn block_end(&self, ix: usize) -> usize {
        self.starts
            .get(ix + 1)
            .copied()
            .unwrap_or(self.line_count)
            .max(self.starts[ix] + 1)
    }
}

/// Everything computed from one version of the source.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub stats: Stats,
    pub blocks: BlockIndex,
    pub outline: Vec<Heading>,
    pub content_hash: u64,
    /// `\[ \]` or `\( \)` math outside code (not looked for past
    /// `FULL_PARSE_LIMIT`).
    pub bracket_math: bool,
}

/// Past this size the source is not parsed as Markdown: a full mdast walk
/// on ~1 MB docs dominated RSS (~1 GB class). 512 KB keeps common notes on
/// the fast path while megabyte files fall back to a line scan for `#`
/// headings (preview shows only the beginning; no block index).
pub const FULL_PARSE_LIMIT: usize = 512 * 1024;

impl Analysis {
    pub fn of(source: &str) -> Self {
        let stats = Stats::of(source);
        let mut starts = Vec::new();
        let mut outline = Vec::new();

        if source.len() > FULL_PARSE_LIMIT {
            return Self {
                stats,
                blocks: BlockIndex::new(Vec::new(), stats.lines),
                outline: scan_atx_headings(source),
                content_hash: content_hash(source),
                bracket_math: false,
            };
        }

        if let Ok(Node::Root(root)) = markdown::to_mdast(source, &preview_parse_options()) {
            for node in &root.children {
                let Some(position) = node.position() else {
                    continue;
                };
                let line = position.start.line.saturating_sub(1);
                starts.push(line);
                if let Node::Heading(heading) = node {
                    let text = plain_text(&heading.children);
                    if !text.is_empty() {
                        outline.push(Heading {
                            level: heading.depth,
                            text,
                            line,
                        });
                    }
                }
            }
        }

        Self {
            stats,
            blocks: BlockIndex::new(starts, stats.lines),
            outline,
            content_hash: content_hash(source),
            bracket_math: crate::math_delims::has_bracket_math(source),
        }
    }
}

/// The `#` headings of `source`, outside fenced code, found line by line.
fn scan_atx_headings(source: &str) -> Vec<Heading> {
    let mut scanner = HeadingScanner::default();
    for line in source.lines() {
        scanner.line(line);
    }
    scanner.outline
}

/// Finds `#` headings one line at a time, so a rope can be scanned chunk by
/// chunk without being copied into one string.
#[derive(Default)]
struct HeadingScanner {
    outline: Vec<Heading>,
    fence: Option<&'static str>,
    line_ix: usize,
}

impl HeadingScanner {
    fn line(&mut self, line: &str) {
        let line_ix = self.line_ix;
        self.line_ix += 1;
        let trimmed = line.trim_start();
        if line.len() - trimmed.len() > 3 {
            return;
        }
        for marker in ["```", "~~~"] {
            if trimmed.starts_with(marker) {
                self.fence = match self.fence {
                    Some(open) if open == marker => None,
                    None => Some(marker),
                    other => other,
                };
            }
        }
        if self.fence.is_some() {
            return;
        }
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        if !(1..=6).contains(&level) {
            return;
        }
        let rest = &trimmed[level..];
        if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
            return;
        }
        let text = rest.trim().trim_end_matches('#').trim().to_string();
        if !text.is_empty() {
            self.outline.push(Heading {
                level: level as u8,
                text,
                line: line_ix,
            });
        }
    }
}

/// How much of one line the streamed heading scan keeps: a heading is never
/// longer, and a file of one enormous line is not copied to look at it.
const SCANNED_LINE_BYTES: usize = 1024;

fn push_capped(line: &mut String, text: &str) {
    let room = SCANNED_LINE_BYTES.saturating_sub(line.len());
    if room == 0 {
        return;
    }
    let mut end = text.len().min(room);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    line.push_str(&text[..end]);
}

impl Analysis {
    /// The analysis of a large document read straight from its chunks (a
    /// rope's), the way `of` treats any source past `FULL_PARSE_LIMIT`, but
    /// without the document ever existing as one string. The outline is
    /// only scanned when asked for.
    pub fn of_chunks<'a>(chunks: impl IntoIterator<Item = &'a str>, outline: bool) -> Self {
        let mut counter = StatsCounter::default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut scanner = HeadingScanner::default();
        let mut line = String::new();
        for chunk in chunks {
            // `str::hash` is the bytes then 0xff; SipHash does not depend on
            // how the bytes are split between writes.
            hasher.write(chunk.as_bytes());
            counter.feed(chunk);
            if outline {
                let mut rest = chunk;
                while let Some(newline) = rest.find('\n') {
                    push_capped(&mut line, &rest[..newline]);
                    scanner.line(&line);
                    line.clear();
                    rest = &rest[newline + 1..];
                }
                push_capped(&mut line, rest);
            }
        }
        if !line.is_empty() {
            scanner.line(&line);
        }
        hasher.write_u8(0xff);
        let stats = counter.finish();
        Self {
            stats,
            blocks: BlockIndex::new(Vec::new(), stats.lines),
            outline: scanner.outline,
            content_hash: hasher.finish(),
            bracket_math: false,
        }
    }
}

/// The parse options the preview uses, so block boundaries agree with the
/// rows the preview renders.
pub fn preview_parse_options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.frontmatter = true;
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// A fast, non-cryptographic hash of the document text.
pub fn content_hash(source: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

fn plain_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(&text.value),
            Node::InlineCode(code) => out.push_str(&code.value),
            Node::InlineMath(math) => out.push_str(&math.value),
            other => {
                if let Some(children) = other.children() {
                    out.push_str(&plain_text(children));
                }
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_chunks_agree_with_the_whole_source() {
        let line = "# Title 标题\nsome words, don’t stop\n```\n# not a heading\n```\n## Two ##\n";
        let source = line.repeat(FULL_PARSE_LIMIT / line.len() + 1) + "tail";
        let whole = Analysis::of(&source);
        // Split at awkward places: inside words, lines and characters' bytes.
        let mut chunks = Vec::new();
        let mut start = 0;
        let mut step = 7;
        while start < source.len() {
            let mut end = (start + step).min(source.len());
            while !source.is_char_boundary(end) {
                end += 1;
            }
            chunks.push(&source[start..end]);
            start = end;
            step = step * 3 % 1013 + 1;
        }
        let streamed = Analysis::of_chunks(chunks.iter().copied(), true);
        assert_eq!(streamed.stats, whole.stats);
        assert_eq!(streamed.content_hash, whole.content_hash);
        assert_eq!(streamed.content_hash, content_hash(&source));
        assert_eq!(streamed.outline.len(), whole.outline.len());
        assert_eq!(streamed.outline[1].text, whole.outline[1].text);
        assert_eq!(
            streamed.outline.last().unwrap().line,
            whole.outline.last().unwrap().line
        );
        assert!(
            Analysis::of_chunks([source.as_str()], false)
                .outline
                .is_empty()
        );
        assert_eq!(Analysis::of_chunks([], false).stats, Stats::of(""));
    }

    #[test]
    fn counts_words_across_scripts() {
        let stats = Stats::of("Hello, world! It's a **bold** move.\n");
        assert_eq!(stats.words, 6);
        assert_eq!(stats.lines, 2);

        // Each ideograph counts as one word; Latin runs still count once.
        assert_eq!(Stats::of("你好世界 GPUI").words, 5);
        assert_eq!(Stats::of("").words, 0);
        assert_eq!(Stats::of("").lines, 1);
    }

    #[test]
    fn reading_time_rounds_up() {
        assert_eq!(Stats::default().reading_minutes(), 0);
        let stats = Stats {
            words: 231,
            ..Default::default()
        };
        assert_eq!(stats.reading_minutes(), 2);
    }

    #[test]
    fn indexes_top_level_blocks_and_headings() {
        let source = "# Title\n\nFirst paragraph\nstill first.\n\n## Section `code`\n\n- a\n- b\n";
        let analysis = Analysis::of(source);
        assert_eq!(analysis.blocks.starts, vec![0, 2, 5, 7]);
        assert_eq!(
            analysis.outline,
            vec![
                Heading {
                    level: 1,
                    text: "Title".into(),
                    line: 0
                },
                Heading {
                    level: 2,
                    text: "Section code".into(),
                    line: 5
                },
            ]
        );
    }

    #[test]
    fn maps_lines_to_blocks_and_back() {
        let index = BlockIndex::new(vec![0, 2, 10], 20);
        assert_eq!(index.block_at_line(0.), Some((0, 0.)));
        assert_eq!(index.block_at_line(6.), Some((1, 0.5)));
        assert_eq!(index.block_at_line(15.), Some((2, 0.5)));
        assert_eq!(index.line_at_block(1, 0.5), Some(6.));
        assert_eq!(index.line_at_block(3, 0.), None);
        assert_eq!(BlockIndex::default().block_at_line(3.), None);
    }

    #[test]
    fn hash_tracks_content() {
        assert_eq!(content_hash("a"), content_hash("a"));
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}

#[cfg(test)]
mod large_tests {
    use super::*;

    #[test]
    fn a_large_document_is_scanned_not_parsed() {
        let line = "word ".repeat(20) + "\n";
        let body = line.repeat(FULL_PARSE_LIMIT / line.len() + 1);
        let source = format!("# Top\n\n```\n# not a heading\n```\n{body}## End ##\n");
        let started = std::time::Instant::now();
        let analysis = Analysis::of(&source);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // ~20 words/line × lines past the parse limit (512 KB → ~100k words).
        assert!(
            analysis.stats.words > 80_000,
            "words={}",
            analysis.stats.words
        );
        let outline: Vec<_> = analysis
            .outline
            .iter()
            .map(|heading| (heading.level, heading.text.as_str()))
            .collect();
        assert_eq!(outline, vec![(1, "Top"), (2, "End")]);
    }
}

/// A code symbol for Outline / Go to Symbol (line is 0-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSymbol {
    pub label: String,
    pub line: usize,
    pub level: u8,
}

/// Scan source for top-level-ish `def` / `class` / `fn` / `function` symbols.
/// Used when fold candidates miss short Python functions (fold needs ≥3 lines).
pub fn code_symbols(source: &str) -> Vec<CodeSymbol> {
    let mut items = Vec::new();
    for (line_ix, line) in source.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let level = (indent / 4).min(8) as u8 + 1;
        if let Some(name) = pythonish_def_name(trimmed) {
            items.push(CodeSymbol {
                label: format!("def {name}"),
                line: line_ix,
                level,
            });
            continue;
        }
        if let Some(name) = strip_keyword_name(trimmed, "class ") {
            items.push(CodeSymbol {
                label: format!("class {name}"),
                line: line_ix,
                level,
            });
            continue;
        }
        let mut matched = false;
        for prefix in [
            "pub async fn ",
            "pub(crate) fn ",
            "pub fn ",
            "async fn ",
            "fn ",
        ] {
            if let Some(name) = strip_keyword_name(trimmed, prefix) {
                items.push(CodeSymbol {
                    label: format!("fn {name}"),
                    line: line_ix,
                    level,
                });
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if let Some(name) = strip_keyword_name(trimmed, "function ") {
            items.push(CodeSymbol {
                label: format!("function {name}"),
                line: line_ix,
                level,
            });
        }
    }
    items
}

fn pythonish_def_name(trimmed: &str) -> Option<&str> {
    for prefix in ["async def ", "def "] {
        if let Some(name) = strip_keyword_name(trimmed, prefix) {
            return Some(name);
        }
    }
    None
}

fn strip_keyword_name<'a>(trimmed: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(prefix)?;
    let name = rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod code_symbol_tests {
    use super::*;

    #[test]
    fn python_sample_lists_defs_and_class() {
        let src = "def alpha():\n    pass\n\ndef beta():\n    return 42\n\nclass Gamma:\n    def method(self):\n        return \"g\"\n";
        let syms = code_symbols(src);
        let labels: Vec<_> = syms.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"def alpha"), "{labels:?}");
        assert!(labels.contains(&"def beta"), "{labels:?}");
        assert!(labels.contains(&"class Gamma"), "{labels:?}");
        assert!(labels.contains(&"def method"), "{labels:?}");
    }
}
