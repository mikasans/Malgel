//! Quick Open: the files under the open folder, so Ctrl+P (⌘P) can jump to
//! one by typing part of its path.
//!
//! The walk honours `.gitignore` (plus `.ignore`, the global git ignore and
//! `.git/info/exclude`), stops descending at [`MAX_DEPTH`], and goes deeper
//! only inside folders the sidebar has already opened, so a folder opened by
//! accident cannot drag the palette through a whole vendored tree. The call
//! is cheap enough to run beside the UI rather than in front of it.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// How deep the listing walks on its own.
pub const MAX_DEPTH: usize = 6;
/// The deepest a folder the user opened in the sidebar may pull the walk.
const MAX_OPENED_DEPTH: usize = 24;
/// The most files one listing holds. A writing folder runs to the hundreds and
/// a big repository to tens of thousands; every one of those must reach Quick
/// Open and Find in Files, so this is only a backstop against a folder opened
/// by accident (a home directory, `/`). The walk is ordered, so when it does
/// bite it keeps the same files every time, and callers can tell it did.
pub const MAX_FILES: usize = 200_000;

/// Every file under `root` the palette should offer, by path.
///
/// `show_hidden` follows the sidebar's setting for dot files. `opened` is
/// the set of folders the sidebar currently has open: below [`MAX_DEPTH`]
/// only those are searched further, and everything else stops at the cap.
pub fn files_under(
    root: &Path,
    show_hidden: bool,
    opened: &[PathBuf],
    limit: usize,
) -> Vec<PathBuf> {
    let opened = opened.to_vec();
    let walker = WalkBuilder::new(root)
        .hidden(!show_hidden)
        // Apply ignore rules outside a git repository too, so a folder of
        // notes with a .gitignore is respected as well.
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .follow_links(false)
        // One thread: this runs beside the UI, not in place of it. Sorted, so
        // the listing (and a capped one) is the same on every run.
        .threads(1)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let depth = entry.depth();
            if depth < MAX_DEPTH {
                return true;
            }
            // Below the cap, keep the folders the user opened, the folders
            // on the way down to one, and whatever is inside them.
            depth < MAX_OPENED_DEPTH
                && opened.iter().any(|folder| {
                    entry.path().starts_with(folder) || folder.starts_with(entry.path())
                })
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.flatten() {
        if files.len() >= limit {
            break;
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort_by(|a, b| {
        a.to_string_lossy()
            .to_lowercase()
            .cmp(&b.to_string_lossy().to_lowercase())
    });
    files
}

/// The most entries a typed folder path lists in the picker.
pub const MAX_DIR_ENTRIES: usize = 500;

/// A query typed into Quick Open read as a path, the way VS Code's Go to
/// File accepts `src/main.rs`, `/abs/file.md`, `~/notes.md` or
/// `README.md:12:3` and opens it even when the listing left it out
/// (ignored, too deep, outside the folder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathQuery {
    /// The path as typed, resolved against the folder and the home folder.
    pub path: PathBuf,
    /// A `:line` suffix, 1-based.
    pub line: Option<usize>,
}

/// Read `query` as a path: absolute, `~/…`, or relative to `root`, with an
/// optional `:line[:column]` suffix. Only the text is parsed; nothing is
/// read from disk.
pub fn parse_path_query(query: &str, root: &Path, home: Option<&Path>) -> Option<PathQuery> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    // `file:12` or `file:12:4`. A Windows drive (`C:\…`) has no digits
    // after its colon, so it is left alone.
    let mut text = query;
    let mut line = None;
    let mut parts = query.rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    if parts.len() >= 2
        && parts[1..]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        let first_number = parts[1];
        text = &query[..query.len() - parts[1..].iter().map(|part| part.len() + 1).sum::<usize>()];
        line = first_number.parse().ok();
        if text.is_empty() {
            return None;
        }
    }
    let path = if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        home?.join(rest)
    } else if text == "~" {
        home?.to_path_buf()
    } else {
        let typed = Path::new(text);
        if typed.is_absolute() {
            typed.to_path_buf()
        } else {
            root.join(typed)
        }
    };
    Some(PathQuery { path, line })
}

/// What a typed path names on disk, checked with a single `stat`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Direct {
    File(PathQuery),
    Dir(PathBuf),
}

/// Look the typed path up: a file opens, a folder is listed.
pub fn resolve_direct(query: &str, root: &Path, home: Option<&Path>) -> Option<Direct> {
    let parsed = parse_path_query(query, root, home)?;
    let metadata = std::fs::metadata(&parsed.path).ok()?;
    if metadata.is_dir() {
        Some(Direct::Dir(parsed.path))
    } else if metadata.is_file() {
        Some(Direct::File(parsed))
    } else {
        None
    }
}

/// The folder a partly typed path is being typed into: `src/` itself, or
/// `src` for `src/ma`. Only a query that names a folder (it has a
/// separator) qualifies, so a plain word still searches the listing.
pub fn typed_dir(query: &str, root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let query = query.trim();
    if !query.contains('/') && !query.contains('\\') && !query.starts_with('~') {
        return None;
    }
    let parsed = parse_path_query(query, root, home)?;
    if query.ends_with('/') || query.ends_with('\\') {
        return parsed.path.is_dir().then_some(parsed.path);
    }
    if parsed.path.is_dir() {
        return Some(parsed.path);
    }
    let parent = parsed.path.parent()?.to_path_buf();
    parent.is_dir().then_some(parent)
}

/// One folder's own entries (not its subfolders'), folders first, ignore
/// rules not applied: the user typed this folder, so it is shown as it is.
pub fn dir_entries(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<(bool, PathBuf)> = read
        .flatten()
        .take(limit)
        .map(|entry| {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            (is_dir, entry.path())
        })
        .collect();
    entries.sort_by(|(a_dir, a), (b_dir, b)| {
        b_dir.cmp(a_dir).then_with(|| {
            a.to_string_lossy()
                .to_lowercase()
                .cmp(&b.to_string_lossy().to_lowercase())
        })
    });
    entries.into_iter().map(|(_, path)| path).collect()
}

#[cfg(test)]
mod path_query_tests {
    use super::*;

    #[test]
    fn parses_relative_absolute_home_and_line_suffixes() {
        let root = Path::new("/ws");
        let home = Path::new("/home/me");
        let q = |text| parse_path_query(text, root, Some(home)).unwrap();
        assert_eq!(q("src/main.rs").path, Path::new("/ws/src/main.rs"));
        assert_eq!(q("src/main.rs").line, None);
        assert_eq!(q("/etc/hosts").path, Path::new("/etc/hosts"));
        assert_eq!(q("~/notes.md").path, Path::new("/home/me/notes.md"));
        assert_eq!(q("README.md:12").line, Some(12));
        assert_eq!(q("README.md:12").path, Path::new("/ws/README.md"));
        let with_col = q("a/b.rs:7:3");
        assert_eq!(
            (with_col.path.as_path(), with_col.line),
            (Path::new("/ws/a/b.rs"), Some(7))
        );
        assert_eq!(q("odd:name").path, Path::new("/ws/odd:name"));
        assert!(parse_path_query("  ", root, Some(home)).is_none());
        assert!(parse_path_query(":12", root, Some(home)).is_none());
    }

    #[test]
    fn resolves_ignored_files_and_lists_one_folder() {
        let root = std::env::temp_dir().join(format!("malgel-direct-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build/deep")).unwrap();
        std::fs::write(root.join(".gitignore"), "build/\nsecret.md\n").unwrap();
        std::fs::write(root.join("secret.md"), "x").unwrap();
        std::fs::write(root.join("build/out.txt"), "x").unwrap();
        std::fs::write(root.join("build/deep/inner.txt"), "x").unwrap();

        // The listing leaves the ignored file out; typing its path finds it.
        let listed = files_under(&root, false, &[], MAX_FILES);
        assert!(!listed.iter().any(|path| path.ends_with("secret.md")));
        assert_eq!(
            resolve_direct("secret.md:3", &root, None),
            Some(Direct::File(PathQuery {
                path: root.join("secret.md"),
                line: Some(3)
            }))
        );
        assert_eq!(
            resolve_direct("build", &root, None),
            Some(Direct::Dir(root.join("build")))
        );
        assert_eq!(resolve_direct("missing.md", &root, None), None);

        // A typed folder lists its own entries only, folders first.
        assert_eq!(typed_dir("build/ou", &root, None), Some(root.join("build")));
        assert_eq!(typed_dir("build/", &root, None), Some(root.join("build")));
        assert_eq!(typed_dir("plainword", &root, None), None);
        assert_eq!(
            dir_entries(&root.join("build"), MAX_DIR_ENTRIES),
            vec![root.join("build/deep"), root.join("build/out.txt")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
    }

    fn relative(root: &Path, files: Vec<PathBuf>) -> Vec<String> {
        files
            .into_iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("malgel-quick-open-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn files_under_respects_gitignore() {
        let dir = scratch("gitignore");
        std::fs::write(dir.join(".gitignore"), "build/\nsecret.md\n").unwrap();
        touch(&dir, "top.md");
        touch(&dir, "notes/a.md");
        touch(&dir, "secret.md");
        touch(&dir, "build/out.md");

        assert_eq!(
            relative(&dir, files_under(&dir, false, &[], 100)),
            ["notes/a.md", "top.md"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_under_stops_at_the_depth_cap_unless_the_folder_was_opened() {
        let dir = scratch("depth");
        touch(&dir, "top.md");
        touch(&dir, "a/b/c/d/e/f/g/h/deep.md");

        // Eight levels down is past the cap, so it is not listed…
        assert_eq!(
            relative(&dir, files_under(&dir, false, &[], 100)),
            ["top.md"]
        );
        // …until the sidebar has opened the folder holding it.
        let opened = [dir.join("a/b/c/d/e/f/g")];
        assert_eq!(
            relative(&dir, files_under(&dir, false, &opened, 100)),
            ["a/b/c/d/e/f/g/h/deep.md", "top.md"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_under_follows_the_show_hidden_setting_and_the_limit() {
        let dir = scratch("hidden");
        touch(&dir, "top.md");
        touch(&dir, ".hidden.md");

        assert_eq!(
            relative(&dir, files_under(&dir, false, &[], 100)),
            ["top.md"]
        );
        assert_eq!(
            relative(&dir, files_under(&dir, true, &[], 100)),
            [".hidden.md", "top.md"]
        );
        assert_eq!(files_under(&dir, true, &[], 1).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
