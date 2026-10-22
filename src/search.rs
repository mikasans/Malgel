//! Find in Files: the text under the open folder, so Ctrl+Shift+F can search
//! every file at once, the way VS Code's Search view does.
//!
//! Only a file's text is searched. A file that is not UTF-8, or that holds a
//! NUL byte — a binary — is skipped rather than guessed at, and one that
//! cannot be read is passed over, the way VS Code leaves unreadable files out
//! of its results.

use std::path::PathBuf;

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

/// VS Code's smart case: a query with no uppercase letter matches either case,
/// one that has an uppercase letter matches exactly. That way `smoke` finds
/// `SMOKE`, while `SMOKE` finds only the shouty one.
pub fn case_sensitive(query: &str) -> bool {
    query.chars().any(char::is_uppercase)
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

/// Where `query` starts in `line` (in characters), under the smart-case rule.
fn line_match(line: &str, query: &str, sensitive: bool) -> Option<usize> {
    if sensitive {
        // `str::find` counts bytes; the column counts characters.
        line.find(query).map(|bytes| line[..bytes].chars().count())
    } else {
        find_ignoring_case(line, query)
    }
}

/// Every line of `text` that holds `query`, as `(line, column, text)`, at most
/// `limit` of them.
pub fn search_text(text: &str, query: &str, limit: usize) -> Vec<(usize, usize, String)> {
    let mut hits = Vec::new();
    if query.is_empty() || limit == 0 {
        return hits;
    }
    let sensitive = case_sensitive(query);
    for (ix, line) in text.lines().enumerate() {
        if hits.len() >= limit {
            break;
        }
        if let Some(column) = line_match(line, query, sensitive) {
            hits.push((ix + 1, column + 1, line.trim().to_string()));
        }
    }
    hits
}

/// How many lines of `text` hold `query` — what [`search_text`] would list
/// with no limit, without building the rows.
pub fn count_lines(text: &str, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    let sensitive = case_sensitive(query);
    if !sensitive && query.is_ascii() && text.is_ascii() {
        let query = query.to_ascii_lowercase();
        return text
            .lines()
            .filter(|line| line.to_ascii_lowercase().contains(&query))
            .count();
    }
    text.lines()
        .filter(|line| line_match(line, query, sensitive).is_some())
        .count()
}

/// Search `files` in the order given, at most `limit` hits in total.
#[cfg(test)]
pub fn search_files(files: &[PathBuf], query: &str, limit: usize) -> Vec<Hit> {
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
    query: &str,
    limit: usize,
    per_file: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> FolderSearch {
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
        let count = count_lines(&text, query);
        if count == 0 {
            continue;
        }
        out.files_matched += 1;
        out.total_hits += count;
        out.file_counts.insert(path.clone(), count);
        let room = limit.saturating_sub(out.hits.len()).min(per_file);
        if room > 0 {
            out.hits
                .extend(
                    search_text(&text, query, room)
                        .into_iter()
                        .map(|(line, column, text)| Hit {
                            path: path.clone(),
                            line,
                            column,
                            text,
                        }),
                );
        }
    }
    out.limited = out.hits.len() < out.total_hits;
    out
}

/// A file's text, or `None` when it cannot be read or is not UTF-8 text.
fn read_text(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// How many occurrences of `find` the files hold, and in how many files,
/// with the same smart-case rule Replace uses. Uncapped: this is what a
/// Replace All is about to change.
pub fn count_in_files(files: &[PathBuf], find: &str) -> (usize, usize) {
    let (mut occurrences, mut matched) = (0, 0);
    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };
        let n = count_in_text(&text, find);
        if n > 0 {
            occurrences += n;
            matched += 1;
        }
    }
    (occurrences, matched)
}

fn count_in_text(text: &str, find: &str) -> usize {
    if find.is_empty() {
        return 0;
    }
    if case_sensitive(find) {
        return text.matches(find).count();
    }
    // Cheap pre-check before the character-wise walk.
    if find.is_ascii() && text.is_ascii() {
        return text
            .to_ascii_lowercase()
            .matches(&find.to_ascii_lowercase())
            .count();
    }
    replace_in_text(text, find, "").1
}

/// Replace every occurrence of `find` in `text` with `replace`, using the same
/// smart-case rule as [`search_text`]. Returns `(new_text, replacements)`.
pub fn replace_in_text(text: &str, find: &str, replace: &str) -> (String, usize) {
    if find.is_empty() {
        return (text.to_string(), 0);
    }
    let sensitive = case_sensitive(find);
    if sensitive {
        let count = text.matches(find).count();
        return (text.replace(find, replace), count);
    }
    // Case-insensitive: walk character-wise so columns stay coherent.
    let needle: Vec<char> = find.chars().collect();
    let hay: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut i = 0;
    while i < hay.len() {
        if i + needle.len() <= hay.len()
            && hay[i..i + needle.len()]
                .iter()
                .zip(&needle)
                .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
        {
            out.push_str(replace);
            count += 1;
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    (out, count)
}

/// Apply [`replace_in_text`] to each path that can be read as UTF-8 text.
/// Returns `(replacements, files written)`, and the files written.
pub fn replace_in_files(files: &[PathBuf], find: &str, replace: &str) -> (usize, Vec<PathBuf>) {
    let mut total = 0;
    let mut written = Vec::new();
    for path in files {
        let Some(text) = read_text(path) else {
            continue;
        };
        if count_in_text(&text, find) == 0 {
            continue;
        }
        let (next, n) = replace_in_text(&text, find, replace);
        if n == 0 {
            continue;
        }
        if std::fs::write(path, next.as_bytes()).is_ok() {
            total += n;
            written.push(path.clone());
        }
    }
    (total, written)
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
    fn a_lowercase_query_matches_either_case_and_an_uppercase_one_does_not() {
        let text = "first SMOKE\nsecond smoke\nthird has none\n";
        assert_eq!(
            search_text(text, "smoke", 10),
            [
                (1, 7, "first SMOKE".to_string()),
                (2, 8, "second smoke".to_string())
            ]
        );
        assert_eq!(
            search_text(text, "SMOKE", 10),
            [(1, 7, "first SMOKE".to_string())]
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
        assert_eq!(count_lines("Fn a\nfn b\n", "Fn"), 1);
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
        assert_eq!(count_in_files(&files, "Alpha"), (4, 4));
        let (n, written) = replace_in_files(&files, "alpha", "gamma");
        assert_eq!((n, written.len()), (8, 4));
        assert_eq!(count_in_files(&files, "alpha"), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replace_in_text_respects_smart_case() {
        let (out, n) = replace_in_text("Foo foo FOO", "foo", "bar");
        assert_eq!(n, 3);
        assert_eq!(out, "bar bar bar");
        let (out, n) = replace_in_text("Foo foo FOO", "Foo", "Bar");
        assert_eq!(n, 1);
        assert_eq!(out, "Bar foo FOO");
    }
}
