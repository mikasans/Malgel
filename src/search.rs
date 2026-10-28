//! Find in Files: the text under the open folder, so Ctrl+Shift+F can search
//! every file at once, the way VS Code's Search view does.
//!
//! Only a file's text is searched. A file that is not UTF-8, or that holds a
//! NUL byte — a binary — is skipped rather than guessed at, and one that
//! cannot be read is passed over, the way VS Code leaves unreadable files out
//! of its results.

use gpui_kit::base::input::{SearchOptions, SearchQuery};
use std::{ops::Range, path::PathBuf};

/// The most hits one search hands the window, as VS Code's
/// `search.maxResults` does. Only the hit count is capped — never the files
/// walked — and a capped search says so ([`FolderSearch::limited`]).
pub const MAX_HITS: usize = 10_000;

/// One line of one file that holds the query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    /// 1-based, so it reads the way editor line numbers do.
    pub line: usize,
    /// 1-based column of the match within its line, counted in characters the
    /// way an editor's caret is, for the row.
    pub column: usize,
    /// The line itself, trimmed, for the row.
    pub text: String,
}

/// Where `query` starts in `line`, comparing without case. Returns the index
/// of the first matching character, so the caller can point at the column.
fn find_ignoring_case(line: &str, query: &str) -> Option<usize> {
    let wanted: Vec<char> = query.chars().collect();
    if wanted.is_empty() {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    if wanted.len() > chars.len() {
        return None;
    }
    (0..=chars.len() - wanted.len()).find(|start| {
        chars[*start..*start + wanted.len()]
            .iter()
            .zip(&wanted)
            .all(|(found, wanted)| found.to_lowercase().eq(wanted.to_lowercase()))
    })
}

/// The bytes of `text` the first case-insensitive match of `query` covers.
pub fn match_bytes(text: &str, query: &str) -> Option<std::ops::Range<usize>> {
    let start = find_ignoring_case(text, query)?;
    let (from, _) = text.char_indices().nth(start)?;
    let len: usize = text[from..]
        .chars()
        .take(query.chars().count())
        .map(char::len_utf8)
        .sum();
    Some(from..from + len)
}

/// The Search view's toggles, as VS Code's: Match Case (Aa), Match Whole
/// Word (ab) and Use Regular Expression (.*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindFlags {
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A compiled Find in Files query: the editor's own matcher
/// ([`SearchQuery`], so both find the same text). Like the editor's Find
/// (and VS Code, whose `search.smartCase` is off by default), case is
/// ignored unless Match Case is on, whatever letters the query has.
#[derive(Clone, Debug, Default)]
pub struct Find {
    query: Option<SearchQuery>,
}

impl Find {
    /// `Err` with the parser's message when `text` is not a valid regex.
    pub fn new(text: &str, flags: FindFlags) -> Result<Self, String> {
        let options = SearchOptions {
            case_sensitive: flags.match_case,
            whole_word: flags.whole_word,
            regex: flags.regex,
        };
        Ok(Self {
            query: SearchQuery::new(text, options)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.query.is_none()
    }

    /// Every match in `text`, as byte ranges.
    fn matches<'a>(&'a self, text: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        self.query
            .iter()
            .flat_map(move |query| query.find_iter(text))
    }

    /// The first match per line, as `(line index, line start, match start)`.
    fn line_starts<'a>(
        &'a self,
        text: &'a str,
    ) -> impl Iterator<Item = (usize, usize, usize)> + 'a {
        let (mut line, mut line_start, mut scanned) = (0usize, 0usize, 0usize);
        let mut last_line = None;
        self.matches(text).filter_map(move |m| {
            let seg = &text[scanned..m.start];
            if let Some(nl) = seg.rfind('\n') {
                line += newlines(seg);
                line_start = scanned + nl + 1;
            }
            scanned = m.start;
            if last_line == Some(line) {
                return None;
            }
            last_line = Some(line);
            Some((line, line_start, m.start))
        })
    }
}

/// A literal query ignoring case: what a typed query means with every
/// toggle off.
impl From<&str> for Find {
    fn from(text: &str) -> Self {
        Find::new(text, FindFlags::default()).unwrap_or_default()
    }
}

impl From<&String> for Find {
    fn from(text: &String) -> Self {
        text.as_str().into()
    }
}

impl From<&Find> for Find {
    fn from(find: &Find) -> Self {
        find.clone()
    }
}

fn newlines(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

/// Every line of `text` that holds `query`, as `(line, column, text)`, at most
/// `limit` of them. A match may run over several lines (a regex with `\n`):
/// the hit is the line it starts on.
pub fn search_text(
    text: &str,
    query: impl Into<Find>,
    limit: usize,
) -> Vec<(usize, usize, String)> {
    let find = query.into();
    if limit == 0 {
        return Vec::new();
    }
    find.line_starts(text)
        .take(limit)
        .map(|(line, line_start, start)| {
            let line_end = text[line_start..]
                .find('\n')
                .map_or(text.len(), |n| line_start + n);
            let column = text[line_start..start].chars().count();
            (
                line + 1,
                column + 1,
                text[line_start..line_end].trim().to_string(),
            )
        })
        .collect()
}

/// How many lines of `text` hold `query` — what [`search_text`] would list
/// with no limit, without building the rows.
pub fn count_lines(text: &str, query: impl Into<Find>) -> usize {
    query.into().line_starts(text).count()
}

/// Search `files` in the order given, at most `limit` hits in total.
#[cfg(test)]
pub fn search_files(files: &[PathBuf], query: impl Into<Find>, limit: usize) -> Vec<Hit> {
    search_folder_files(
        files,
        query,
        limit,
        limit,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .hits
}

/// The most hits one file shows, so a single huge file cannot take the
/// whole [`MAX_HITS`] and hide every other file's results.
pub const MAX_HITS_PER_FILE: usize = 1_000;

/// What a search of a folder found, and how much of the folder it read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderSearch {
    /// The hits shown: at most [`MAX_HITS`], at most [`MAX_HITS_PER_FILE`]
    /// from one file.
    pub hits: Vec<Hit>,
    /// Every matching line in the folder, shown or not.
    pub total_hits: usize,
    /// Matching lines per file, shown or not.
    pub file_counts: std::collections::HashMap<PathBuf, usize>,
    /// Files with at least one hit.
    pub files_matched: usize,
    /// Files read before the search stopped (all of them unless cancelled).
    pub files_searched: usize,
    /// Files the folder listing holds.
    pub files_total: usize,
    /// Fewer hits are shown than the folder holds.
    pub limited: bool,
}

/// Search every one of `files` in order. Every match is counted; at most
/// `limit` are kept as rows, and at most `per_file` from any one file.
pub fn search_folder_files(
    files: &[PathBuf],
    query: impl Into<Find>,
    limit: usize,
    per_file: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> FolderSearch {
    let query = query.into();
    let mut out = FolderSearch {
        files_total: files.len(),
        ..FolderSearch::default()
    };
    if query.is_empty() {
        return out;
    }
    for path in files {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        out.files_searched += 1;
        let Some(text) = read_text(path) else {
            continue;
        };
        let count = count_lines(&text, &query);
        if count == 0 {
            continue;
        }
        out.files_matched += 1;
        out.total_hits += count;
        out.file_counts.insert(path.clone(), count);
        let room = limit.saturating_sub(out.hits.len()).min(per_file);
        if room > 0 {
            out.hits
                .extend(search_text(&text, &query, room).into_iter().map(
                    |(line, column, text)| Hit {
                        path: path.clone(),
                        line,
                        column,
                        text,
                    },
                ));
        }
    }
    out.limited = out.hits.len() < out.total_hits;
    out
}

/// A file's text, or `None` when it cannot be read as text.
fn read_text(path: &std::path::Path) -> Option<String> {
    read_loaded(path).ok().map(|loaded| loaded.text)
}

/// Why a file's text could not be searched.
enum Unreadable {
    /// Not there, or binary: not text at all, so not worth a mention.
    NotText,
    /// Text in no encoding Malgel recognises with confidence: a Replace
    /// would write U+FFFD over its unknown bytes.
    Lossy,
}

/// A file decoded the way the editor opens it (UTF-8, UTF-8 with BOM,
/// UTF-16, GBK, Big5, Shift JIS, Latin-1), with its line ending and
/// encoding so a Replace can write it back the same way.
fn read_loaded(path: &std::path::Path) -> Result<crate::document::LoadedText, Unreadable> {
    use crate::document::Encoding;
    let bytes = std::fs::read(path).map_err(|_| Unreadable::NotText)?;
    let utf16 = matches!(
        Encoding::from_bom(&bytes),
        Encoding::Utf16Le | Encoding::Utf16Be
    );
    if !utf16 && bytes.contains(&0) {
        return Err(Unreadable::NotText);
    }
    match crate::document::decode(bytes) {
        Ok(loaded) if loaded.lossy => Err(Unreadable::Lossy),
        Ok(loaded) => Ok(loaded),
        Err(_) => Err(Unreadable::NotText),
    }
}

/// How many occurrences of `find` the files hold, and in how many files,
/// with the same case rule Replace uses. Uncapped: this is what a
/// Replace All is about to change.
pub fn count_in_files(files: &[PathBuf], find: impl Into<Find>) -> (usize, usize) {
    let find = find.into();
    let (mut occurrences, mut matched) = (0, 0);
    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };
        let n = count_in_text(&text, &find);
        if n > 0 {
            occurrences += n;
            matched += 1;
        }
    }
    (occurrences, matched)
}

fn count_in_text(text: &str, find: &Find) -> usize {
    find.matches(text).count()
}

/// Replace every match of `find` in `text` with `replace` (with a regex,
/// `$1`, `${name}`, `$&` and `\n` expand, as in the editor's Replace).
/// Returns `(new_text, replacements)`.
pub fn replace_in_text(text: &str, find: impl Into<Find>, replace: &str) -> (String, usize) {
    let find = find.into();
    let Some(query) = find.query.as_ref() else {
        return (text.to_string(), 0);
    };
    let mut out = String::with_capacity(text.len());
    let (mut count, mut last) = (0, 0);
    for range in query.find_iter(text) {
        out.push_str(&text[last..range.start]);
        out.push_str(&query.replacement(text, range.clone(), replace));
        last = range.end;
        count += 1;
    }
    out.push_str(&text[last..]);
    (out, count)
}

/// What a Replace All did.
#[derive(Debug, Default, PartialEq)]
pub struct Replaced {
    pub occurrences: usize,
    pub written: Vec<PathBuf>,
    /// Files left alone: text in no encoding read with confidence, a
    /// replacement their encoding cannot hold, or a failed write.
    pub skipped: usize,
}

/// Apply [`replace_in_text`] to each file that reads as text, writing it
/// back atomically in its own encoding and line ending.
pub fn replace_in_files(files: &[PathBuf], find: impl Into<Find>, replace: &str) -> Replaced {
    let find = find.into();
    let mut out = Replaced::default();
    for path in files {
        let loaded = match read_loaded(path) {
            Ok(loaded) => loaded,
            Err(Unreadable::Lossy) => {
                out.skipped += 1;
                continue;
            }
            Err(Unreadable::NotText) => continue,
        };
        if count_in_text(&loaded.text, &find) == 0 {
            continue;
        }
        let (next, n) = replace_in_text(&loaded.text, &find, replace);
        if n == 0 {
            continue;
        }
        match crate::document::save_encoded(path, &next, loaded.line_ending, loaded.encoding) {
            Ok(()) => {
                out.occurrences += n;
                out.written.push(path.clone());
            }
            Err(_) => out.skipped += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("malgel-search-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_cancelled_search_reads_no_further() {
        let dir = scratch("cancel");
        let file = dir.join("a.md");
        std::fs::write(&file, "needle\n").unwrap();
        let files = vec![file.clone(), file];
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(
            search_folder_files(&files, "needle", 10, 10, &cancel)
                .hits
                .is_empty()
        );
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            search_folder_files(&files, "needle", 10, 10, &cancel)
                .hits
                .len(),
            2
        );
    }

    #[test]
    fn a_query_matches_either_case_whatever_its_letters() {
        let text = "first SMOKE\nsecond smoke\nthird has none\n";
        assert_eq!(
            search_text(text, "smoke", 10),
            [
                (1, 7, "first SMOKE".to_string()),
                (2, 8, "second smoke".to_string())
            ]
        );
        // No smart case: the editor's Find ignores case until Aa is on.
        assert_eq!(
            search_text(text, "SMOKE", 10),
            search_text(text, "smoke", 10)
        );
        assert!(search_text(text, "smoke", 0).is_empty());
        assert!(search_text(text, "", 10).is_empty());
    }

    #[test]
    fn the_line_and_column_are_one_based_and_the_column_is_not_ascii_blind() {
        // 「日本語」 is three characters and nine bytes: the column counts
        // characters, the way an editor's caret does.
        let text = "no\n日本語 needle here\n";
        assert_eq!(
            search_text(text, "needle", 10),
            [(2, 5, "日本語 needle here".to_string())]
        );
        // A non-ASCII query matches regardless of case, too.
        assert_eq!(
            search_text("ÉCOLE\nécole\n", "école", 10),
            [(1, 1, "ÉCOLE".to_string()), (2, 1, "école".to_string())]
        );
    }

    #[test]
    fn the_limit_stops_the_walk_where_it_says() {
        let text = (1..=10)
            .map(|ix| format!("hit {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(search_text(&text, "hit", 3).len(), 3);
        assert_eq!(search_text(&text, "hit", 100).len(), 10);
    }

    #[test]
    fn search_files_covers_every_file_and_skips_what_is_not_text() {
        let dir = scratch("files");
        let first = dir.join("first.md");
        let second = dir.join("second.md");
        let binary = dir.join("third.md");
        std::fs::write(&first, "alpha\nneedle one\n").unwrap();
        std::fs::write(&second, "needle two\nbeta\n").unwrap();
        std::fs::write(&binary, b"needle\x00binary").unwrap();

        let files = [first.clone(), second.clone(), binary];
        let hits = search_files(&files, "needle", 10);
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.path.clone(), hit.line, hit.text.clone()))
                .collect::<Vec<_>>(),
            [
                (first.clone(), 2, "needle one".to_string()),
                (second.clone(), 1, "needle two".to_string()),
            ]
        );
        // The cap is over the whole walk, not one file.
        assert_eq!(search_files(&files, "needle", 1).len(), 1);
        // A file that is not there is passed over, not an error.
        assert!(search_files(&[dir.join("gone.md")], "needle", 10).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_search_reads_every_file_and_counts_past_the_caps() {
        let dir = scratch("folder");
        let mut files: Vec<PathBuf> = (0..30)
            .map(|ix| {
                let path = dir.join(format!("f{ix:02}.rs"));
                let body = if ix == 0 || ix == 29 {
                    format!("fn token_{ix}() {{}}\n")
                } else {
                    "fn other() {}\n".to_string()
                };
                std::fs::write(&path, body).unwrap();
                path
            })
            .collect();
        // One big file first: it must not eat the whole cap.
        let big = dir.join("big.rs");
        std::fs::write(&big, "fn f() {}\n".repeat(500)).unwrap();
        files.insert(0, big.clone());
        let none = std::sync::atomic::AtomicBool::new(false);
        // The first and last file are both reached: nothing caps the walk.
        let found = search_folder_files(&files, "token_", 100, 10, &none);
        assert_eq!(found.hits.len(), 2);
        assert_eq!((found.files_searched, found.files_total), (31, 31));
        assert_eq!((found.files_matched, found.total_hits), (2, 2));
        assert!(!found.limited);
        // Every match is counted; rows are capped per file and in total.
        let found = search_folder_files(&files, "fn", 25, 10, &none);
        assert_eq!(found.total_hits, 530);
        assert_eq!(found.files_matched, 31);
        assert_eq!(found.file_counts[&big], 500);
        assert_eq!(found.hits.len(), 25);
        assert_eq!(found.hits.iter().filter(|hit| hit.path == big).count(), 10);
        assert!(found.limited);
        assert_eq!(count_lines("Fn a\nfn b\nnone\n", "fn"), 2);
        assert_eq!(count_lines("Fn a\nfn b\n", "Fn"), 2);
        assert_eq!(match_bytes("src/M099.rs", "m09"), Some(4..7));
        assert_eq!(match_bytes("ab", "B"), Some(1..2));
        assert_eq!(match_bytes("éa", "A"), Some(2..3));
        assert_eq!(match_bytes("abc", "abc"), Some(0..3));
        assert_eq!(match_bytes("abc", "x"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counting_and_replacing_agree_on_every_file() {
        let dir = scratch("replace-all");
        let files: Vec<PathBuf> = (0..12)
            .map(|ix| {
                let path = dir.join(format!("r{ix}.md"));
                let body = if ix % 3 == 0 {
                    "Alpha alpha\n"
                } else {
                    "beta\n"
                };
                std::fs::write(&path, body).unwrap();
                path
            })
            .collect();
        assert_eq!(count_in_files(&files, "alpha"), (8, 4));
        assert_eq!(count_in_files(&files, "Alpha"), (8, 4), "no smart case");
        let replaced = replace_in_files(&files, "alpha", "gamma");
        assert_eq!((replaced.occurrences, replaced.written.len()), (8, 4));
        assert_eq!(replaced.skipped, 0);
        assert_eq!(count_in_files(&files, "alpha"), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_in_text_ignores_case_like_the_editor_find() {
        let (out, n) = replace_in_text("Foo foo FOO", "foo", "bar");
        assert_eq!(n, 3);
        assert_eq!(out, "bar bar bar");
        // An uppercase letter does not switch on Match Case (no smart case):
        // the editor's Find finds all three too.
        let (out, n) = replace_in_text("Foo foo FOO", "Foo", "Bar");
        assert_eq!(n, 3);
        assert_eq!(out, "Bar Bar Bar");
        let case = FindFlags {
            match_case: true,
            ..Default::default()
        };
        let (out, n) = replace_in_text("Foo foo FOO", Find::new("Foo", case).unwrap(), "Bar");
        assert_eq!(n, 1);
        assert_eq!(out, "Bar foo FOO");
    }

    #[test]
    fn regex_whole_word_and_match_case_share_the_editor_matcher() {
        let regex = FindFlags {
            regex: true,
            ..Default::default()
        };
        let find = Find::new(r"foo\((\w+), ?(\w+)\)", regex).unwrap();
        let text = "a = foo(x, y);\nb = foo(cc,dd);\nfoo()\n";
        assert_eq!(
            search_text(text, &find, 10),
            [
                (1, 5, "a = foo(x, y);".to_string()),
                (2, 5, "b = foo(cc,dd);".to_string())
            ]
        );
        let (out, n) = replace_in_text(text, &find, "bar($2, $1)");
        assert_eq!(n, 2);
        assert_eq!(out, "a = bar(y, x);\nb = bar(dd, cc);\nfoo()\n");
        // Whole word, and Match Case.
        let word = FindFlags {
            whole_word: true,
            ..Default::default()
        };
        assert_eq!(
            count_lines("food\nfoo.\nFoo\n", Find::new("foo", word).unwrap()),
            2
        );
        let case = FindFlags {
            match_case: true,
            ..Default::default()
        };
        assert_eq!(
            count_lines("food\nFoo\n", Find::new("foo", case).unwrap()),
            1
        );
        // A match over two lines is a hit on the line it starts.
        let two = Find::new(r"one\ntwo", regex).unwrap();
        assert_eq!(
            search_text("zero\none\ntwo\n", &two, 10),
            [(2, 1, "one".to_string())]
        );
        // Two matches on one line are one hit row, two occurrences.
        let a = Find::from("a");
        assert_eq!(
            (count_lines("a a\nb\na", &a), count_in_text("a a\nb\na", &a)),
            (2, 3)
        );
        // An invalid regex is an error to show, never a panic.
        assert!(Find::new("(", regex).is_err());
        assert!(Find::new("(", FindFlags::default()).is_ok());
    }

    #[test]
    fn regex_replace_in_files_keeps_the_encoding() {
        use crate::document::{Encoding, LineEnding, encode};
        let dir = scratch("regex-files");
        let gbk = dir.join("gbk.txt");
        std::fs::write(
            &gbk,
            encode("调用 foo(甲, 乙)\n", LineEnding::CrLf, Encoding::Gbk).unwrap(),
        )
        .unwrap();
        let find = Find::new(
            r"foo\((\w+), ?(\w+)\)",
            FindFlags {
                regex: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_in_files(std::slice::from_ref(&gbk), &find), (1, 1));
        let replaced = replace_in_files(std::slice::from_ref(&gbk), &find, "bar($2, $1)");
        assert_eq!(replaced.written, vec![gbk.clone()]);
        assert_eq!(
            std::fs::read(&gbk).unwrap(),
            encode("调用 bar(乙, 甲)\n", LineEnding::CrLf, Encoding::Gbk).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The acceptance bar for regex Find: 100k lines in under 200 ms in a
    /// release build, 400 ms in the debug build CI runs (the regex crates
    /// are optimized there too, see Cargo.toml).
    #[test]
    fn a_regex_find_over_100k_lines_takes_under_200ms() {
        let text: String = (0..100_000)
            .map(|i| format!("let value_{i} = foo(a{i}, b{i}); // line {i}\n"))
            .collect();
        let find = Find::new(
            r"foo\((\w+), ?(\w+)\)",
            FindFlags {
                regex: true,
                ..Default::default()
            },
        )
        .unwrap();
        // Best of three, so one descheduled moment on a busy CI runner
        // (tests run in parallel) does not fail the bar.
        let took = (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                assert_eq!(count_lines(&text, &find), 100_000);
                started.elapsed()
            })
            .min()
            .unwrap();
        eprintln!("regex find, 100k lines (best of 3): {took:?}");
        // 200 ms is the bar for a release build; a debug test build (what
        // CI runs) still has an unoptimized caller, so it gets headroom.
        let limit_ms = if cfg!(debug_assertions) { 400 } else { 200 };
        assert!(
            took.as_millis() < limit_ms,
            "regex find over 100k lines took {took:?}"
        );
    }

    /// GBK, Latin-1 and UTF-16 files are found and replaced, and keep their
    /// encoding and line endings; text in no known encoding is skipped and
    /// counted, never written over with U+FFFD; a replacement the
    /// encoding can't hold leaves the file alone.
    #[test]
    fn legacy_encoded_files_are_found_and_replaced_in_their_encoding() {
        use crate::document::{Encoding, LineEnding, encode};
        let dir = scratch("encodings");
        let gbk = dir.join("gbk.txt");
        let latin = dir.join("latin.txt");
        let utf16 = dir.join("utf16.txt");
        let unknown = dir.join("unknown.txt");
        std::fs::write(
            &gbk,
            encode("第一行 needle\n第二行\n", LineEnding::CrLf, Encoding::Gbk).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &latin,
            encode("café needle\n", LineEnding::Lf, Encoding::Latin1).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &utf16,
            encode("needle ☃\n", LineEnding::Lf, Encoding::Utf16Le).unwrap(),
        )
        .unwrap();
        std::fs::write(&unknown, b"needle \xff\xfe\x81 junk\n").unwrap();
        let files = vec![gbk.clone(), latin.clone(), utf16.clone(), unknown.clone()];
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let found = search_folder_files(&files, "needle", 100, 10, &cancel);
        assert!(found.files_matched >= 3, "{found:?}");
        for path in [&gbk, &latin, &utf16] {
            assert!(
                found.hits.iter().any(|hit| &hit.path == path),
                "{path:?} not found"
            );
        }
        let original_unknown = std::fs::read(&unknown).unwrap();
        let replaced = replace_in_files(&files, "needle", "針");
        // Latin-1 can't hold 針: that file is skipped, as is the unknown one.
        assert_eq!(replaced.written, vec![gbk.clone(), utf16.clone()]);
        assert_eq!(replaced.skipped, 2);
        assert_eq!(
            std::fs::read(&gbk).unwrap(),
            encode("第一行 針\n第二行\n", LineEnding::CrLf, Encoding::Gbk).unwrap()
        );
        assert_eq!(
            std::fs::read(&utf16).unwrap(),
            encode("針 ☃\n", LineEnding::Lf, Encoding::Utf16Le).unwrap()
        );
        assert_eq!(std::fs::read(&unknown).unwrap(), original_unknown);
        let replaced = replace_in_files(std::slice::from_ref(&latin), "needle", "thé");
        assert_eq!(replaced.written, vec![latin.clone()]);
        assert_eq!(
            std::fs::read(&latin).unwrap(),
            encode("café thé\n", LineEnding::Lf, Encoding::Latin1).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
