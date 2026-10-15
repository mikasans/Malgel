//! Find in Files: the text under the open folder, so Ctrl+Shift+F can search
//! every file at once, the way VS Code's Search view does.
//!
//! Only a file's text is searched. A file that is not UTF-8, or that holds a
//! NUL byte — a binary — is skipped rather than guessed at, and one that
//! cannot be read is passed over, the way VS Code leaves unreadable files out
//! of its results.

use std::path::PathBuf;

/// The most hits one search hands the window. A common word in a large folder
/// matches tens of thousands of lines; the cap keeps the palette usable.
pub const MAX_HITS: usize = 500;

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
    search_files_until(
        files,
        query,
        limit,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

/// [`search_files`], stopping between files once `cancel` is set, so a
/// search the user dismissed stops reading the folder.
pub fn search_files_until(
    files: &[PathBuf],
    query: &str,
    limit: usize,
    cancel: &std::sync::atomic::AtomicBool,
) -> Vec<Hit> {
    let mut hits = Vec::new();
    if query.is_empty() {
        return hits;
    }
    for path in files {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let left = limit.saturating_sub(hits.len());
        if left == 0 {
            break;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.contains(&0) {
            // A NUL byte means this is not text, whoever named it `.md`.
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (line, column, text) in search_text(&text, query, left) {
            hits.push(Hit {
                path: path.clone(),
                line,
                column,
                text,
            });
        }
    }
    hits
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
        assert!(search_files_until(&files, "needle", 10, &cancel).is_empty());
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(search_files_until(&files, "needle", 10, &cancel).len(), 2);
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
}
