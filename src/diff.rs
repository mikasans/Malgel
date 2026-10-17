//! Line-based compare/diff hunks for Compare with Saved and disk-conflict Compare.

/// One contiguous changed region in a two-pane line diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// 0-based start line in the left (saved/disk) text.
    pub left_start: usize,
    /// Number of left lines in the hunk (0 = pure insert).
    pub left_count: usize,
    /// 0-based start line in the right (buffer) text.
    pub right_start: usize,
    /// Number of right lines in the hunk (0 = pure delete).
    pub right_count: usize,
}

/// Split `text` into lines the way an editor counts them (no phantom trailing empty).
pub fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        out.pop();
    }
    out
}

/// Compute change hunks between `left` (saved/disk) and `right` (buffer).
pub fn hunks(left: &str, right: &str) -> Vec<Hunk> {
    let a = lines(left);
    let b = lines(right);
    let ops = if a.len().saturating_mul(b.len()) > 200_000 {
        greedy_ops(&a, &b)
    } else {
        lcs_ops(&a, &b)
    };
    collapse_hunks(&ops)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Equal);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Delete);
            i += 1;
        } else {
            ops.push(Op::Insert);
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete);
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert);
        j += 1;
    }
    ops
}

fn greedy_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push(Op::Equal);
            i += 1;
            j += 1;
            continue;
        }
        // Look ahead a little for a re-sync.
        let mut synced = false;
        for skip in 1..=8 {
            if i + skip <= a.len()
                && j < b.len()
                && a[i + skip - 1..].first().is_some_and(|l| *l == b[j])
            {
                for _ in 0..skip {
                    ops.push(Op::Delete);
                    i += 1;
                }
                synced = true;
                break;
            }
            if j + skip <= b.len()
                && i < a.len()
                && b[j + skip - 1..].first().is_some_and(|l| *l == a[i])
            {
                for _ in 0..skip {
                    ops.push(Op::Insert);
                    j += 1;
                }
                synced = true;
                break;
            }
        }
        if synced {
            continue;
        }
        if i < a.len() && (j >= b.len() || a.len() - i >= b.len() - j) {
            ops.push(Op::Delete);
            i += 1;
        } else if j < b.len() {
            ops.push(Op::Insert);
            j += 1;
        }
    }
    ops
}

fn collapse_hunks(ops: &[Op]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut li = 0usize;
    let mut ri = 0usize;
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            Op::Equal => {
                li += 1;
                ri += 1;
                i += 1;
            }
            Op::Delete | Op::Insert => {
                let left_start = li;
                let right_start = ri;
                let mut left_count = 0;
                let mut right_count = 0;
                while i < ops.len() && !matches!(ops[i], Op::Equal) {
                    match ops[i] {
                        Op::Delete => {
                            left_count += 1;
                            li += 1;
                        }
                        Op::Insert => {
                            right_count += 1;
                            ri += 1;
                        }
                        Op::Equal => unreachable!(),
                    }
                    i += 1;
                }
                hunks.push(Hunk {
                    left_start,
                    left_count,
                    right_start,
                    right_count,
                });
            }
        }
    }
    hunks
}

/// Format the visible lines for one side of a hunk (with ±2 lines of context).
/// Always returns at least one row when `lines` is non-empty or the hunk exists,
/// so Compare panes never paint an empty body when hunk count ≥ 1.
pub fn pane_view(hunk: &Hunk, lines: &[&str], side_left: bool) -> Vec<String> {
    let (start0, count) = if side_left {
        (hunk.left_start, hunk.left_count)
    } else {
        (hunk.right_start, hunk.right_count)
    };
    let start = start0.saturating_sub(2);
    let end = (start0 + count.max(1) + 2).min(lines.len());
    if start >= end {
        let fallback = start0.min(lines.len().saturating_sub(1));
        if lines.is_empty() {
            return vec!["(empty)".into()];
        }
        return vec![format!("{:>4} │ {}", fallback + 1, lines[fallback])];
    }
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>4} │ {}", start + i + 1, line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_has_no_hunks() {
        assert!(hunks("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn detects_replace() {
        let h = hunks("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(h.len(), 1);
        assert_eq!(
            h[0],
            Hunk {
                left_start: 1,
                left_count: 1,
                right_start: 1,
                right_count: 1,
            }
        );
    }

    #[test]
    fn pure_insert() {
        let h = hunks("a\n", "a\nb\n");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].left_count, 0);
        assert_eq!(h[0].right_count, 1);
        assert_eq!(h[0].right_start, 1);
    }

    #[test]
    fn pure_delete() {
        let h = hunks("a\nb\n", "a\n");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].left_count, 1);
        assert_eq!(h[0].right_count, 0);
    }

    #[test]
    fn pane_view_nonempty_when_hunk_present() {
        let left = "file a\nPROBE-A\n";
        let right = "file b\nPROBE-B\n";
        let h = hunks(left, right);
        assert!(!h.is_empty());
        let left_lines = lines(left);
        let right_lines = lines(right);
        let lv = pane_view(&h[0], &left_lines, true);
        let rv = pane_view(&h[0], &right_lines, false);
        assert!(!lv.is_empty(), "left pane lines must be non-empty");
        assert!(!rv.is_empty(), "right pane lines must be non-empty");
        assert!(
            lv.iter()
                .any(|l| l.contains("file a") || l.contains("PROBE-A"))
        );
        assert!(
            rv.iter()
                .any(|l| l.contains("file b") || l.contains("PROBE-B"))
        );
    }

    #[test]
    fn pane_view_pure_insert_still_shows_context() {
        let h = hunks("a\n", "a\nb\n");
        assert_eq!(h.len(), 1);
        let left_lines = lines("a\n");
        let right_lines = lines("a\nb\n");
        let lv = pane_view(&h[0], &left_lines, true);
        let rv = pane_view(&h[0], &right_lines, false);
        assert!(!lv.is_empty());
        assert!(rv.iter().any(|l| l.contains('b') || l.contains("a")));
    }
}
