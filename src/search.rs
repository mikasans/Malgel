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
        let found = if sensitive {
            // `str::find` counts bytes; the column counts characters.
            line.find(query).map(|bytes| line[..bytes].chars().count())
        } else {
            find_ignoring_case(line, query)
        };
        if let Some(column) = found {
            hits.push((ix + 1, column + 1, line.trim().to_string()));
        }
    }
    hits
}

/// Search `files` in the order given, at most `limit` hits in total.
#[cfg(test)]
pub fn search_files(files: &[PathBuf], query: &str, limit: usize) -> Vec<Hit> {
    search_folder_files(
        files,
        query,
        limit,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .hits
}

/// What a search of a folder found, and how much of the folder it read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderSearch {
    pub hits: Vec<Hit>,
    /// Files with at least one hit.
    pub files_matched: usize,
    /// Files read before the search stopped.
    pub files_searched: usize,
    /// Files the folder listing holds.
    pub files_total: usize,
    /// The hit cap stopped the search before the last file.
    pub limited: bool,
}

/// Search `files` in order, at most `limit` hits, counting how far it got.
pub fn search_folder_files(
    files: &[PathBuf],
    query: &str,
    limit: usize,
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
        let left = limit.saturating_sub(out.hits.len());
        if left == 0 {
            out.limited = true;
            break;
        }
        out.files_searched += 1;
        let Some(text) = read_text(path) else {
            continue;
        };
        let found = search_text(&text, query, left);
        if found.is_empty() {
            continue;
        }
        out.files_matched += 1;
        if found.len() == left {
            // The cap may have cut this file short; whether more files were
            // left is settled on the next turn round the loop.
            out.limited = out.files_searched < files.len()
                || search_text(&text, query, left + 1).len() > left;
        }
        out.hits
            .extend(found.into_iter().map(|(line, column, text)| Hit {
                path: path.clone(),
                line,
                column,
                text,
            }));
    }
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
            search_folder_files(&files, "needle", 10, &cancel)
                .hits
                .is_empty()
        );
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            search_folder_files(&files, "needle", 10, &cancel)
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
    fn a_folder_search_reads_every_file_and_says_when_the_hit_cap_bit() {
        let dir = scratch("folder");
        let files: Vec<PathBuf> = (0..30)
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
        let none = std::sync::atomic::AtomicBool::new(false);
        // The first and last file are both reached: nothing caps the walk.
        let found = search_folder_files(&files, "token_", 100, &none);
        assert_eq!(found.hits.len(), 2);
        assert_eq!((found.files_searched, found.files_total), (30, 30));
        assert_eq!(found.files_matched, 2);
        assert!(!found.limited);
        // Only the hit cap stops it, and then it says how far it got.
        let found = search_folder_files(&files, "fn", 5, &none);
        assert_eq!(found.hits.len(), 5);
        assert!(found.limited);
        assert_eq!(found.files_searched, 5);
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
