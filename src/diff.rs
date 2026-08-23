//! Line-diff engine: builds an aligned row model that serves both the
//! side-by-side view and the inline (stacked) view.

use similar::{ChangeTag, TextDiffConfig};
use std::collections::HashSet;
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

/// Columns a tab expands to. The renderer and the width measurement here
/// must agree, or horizontal scrolling would drift on tab-indented files.
pub const TAB_WIDTH: usize = 4;

/// Cap on diff computation. Myers is O(N·D): two large, mostly-different
/// files (a rewritten bundle, generated code) can otherwise take multiple
/// seconds. On timeout `similar` degrades gracefully to a coarser diff
/// instead of hanging the load job.
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

/// Context rows kept visible on each side of a fold.
pub const FOLD_MARGIN: usize = 3;
/// Minimum number of rows a fold must hide to be worth folding.
pub const FOLD_MIN: usize = 4;

/// One visual line of the diff view once folding is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayEntry {
    /// Index into `rows` (side-by-side view) or `inline` (inline view).
    Line(usize),
    /// A folded run of unchanged rows: `rows[start .. start + count]`.
    Fold { start: usize, count: usize },
    /// Header above a fold the user expanded — click it to fold again.
    /// Same `start` key as the [`DisplayEntry::Fold`] it replaced.
    Unfold { start: usize, count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Added,
    Removed,
    /// A removed line paired with an added line (rendered on one screen row
    /// in side-by-side mode, two stacked rows in inline mode).
    Modified,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub kind: RowKind,
    pub old_ln: Option<usize>,
    pub new_ln: Option<usize>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// One visual line of the inline view: which row, and which side of it.
#[derive(Debug, Clone, Copy)]
pub struct InlineEntry {
    pub row: usize,
    pub side: Side,
}

#[derive(Debug, Default)]
pub struct FileDiff {
    pub rows: Vec<Row>,
    pub inline: Vec<InlineEntry>,
    pub additions: usize,
    pub deletions: usize,
    /// Display width of the widest line on either side, tabs expanded —
    /// the bound for horizontal scrolling.
    pub max_width: usize,
}

/// Display width of a diff line, expanding tabs the way the renderer does.
pub fn line_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if c == '\t' {
                TAB_WIDTH
            } else {
                c.width().unwrap_or(0)
            }
        })
        .sum()
}

impl FileDiff {
    pub fn compute(old: Option<&str>, new: Option<&str>) -> Self {
        let old_s = old.unwrap_or("");
        let new_s = new.unwrap_or("");
        let diff = TextDiffConfig::default()
            .timeout(DIFF_TIMEOUT)
            .diff_lines(old_s, new_s);

        let mut rows: Vec<Row> = Vec::new();
        let mut dels: Vec<(usize, String)> = Vec::new();
        let mut inss: Vec<(usize, String)> = Vec::new();
        let mut additions = 0usize;
        let mut deletions = 0usize;

        let flush = |rows: &mut Vec<Row>,
                     dels: &mut Vec<(usize, String)>,
                     inss: &mut Vec<(usize, String)>| {
            let paired = dels.len().min(inss.len());
            for i in 0..paired {
                rows.push(Row {
                    kind: RowKind::Modified,
                    old_ln: Some(dels[i].0),
                    new_ln: Some(inss[i].0),
                    old_text: Some(dels[i].1.clone()),
                    new_text: Some(inss[i].1.clone()),
                });
            }
            for d in dels.iter().skip(paired) {
                rows.push(Row {
                    kind: RowKind::Removed,
                    old_ln: Some(d.0),
                    new_ln: None,
                    old_text: Some(d.1.clone()),
                    new_text: None,
                });
            }
            for a in inss.iter().skip(paired) {
                rows.push(Row {
                    kind: RowKind::Added,
                    old_ln: None,
                    new_ln: Some(a.0),
                    old_text: None,
                    new_text: Some(a.1.clone()),
                });
            }
            dels.clear();
            inss.clear();
        };

        for change in diff.iter_all_changes() {
            let text = change.value().trim_end_matches('\n').to_string();
            match change.tag() {
                ChangeTag::Equal => {
                    flush(&mut rows, &mut dels, &mut inss);
                    rows.push(Row {
                        kind: RowKind::Context,
                        old_ln: change.old_index().map(|i| i + 1),
                        new_ln: change.new_index().map(|i| i + 1),
                        old_text: Some(text.clone()),
                        new_text: Some(text),
                    });
                }
                ChangeTag::Delete => {
                    deletions += 1;
                    dels.push((change.old_index().unwrap_or(0) + 1, text));
                }
                ChangeTag::Insert => {
                    additions += 1;
                    inss.push((change.new_index().unwrap_or(0) + 1, text));
                }
            }
        }
        flush(&mut rows, &mut dels, &mut inss);

        // Degenerate case: both sides empty -> no rows at all.
        if old.is_none() && new.is_none() {
            rows.clear();
        }

        let mut inline = Vec::with_capacity(rows.len() + additions);
        for (i, row) in rows.iter().enumerate() {
            match row.kind {
                RowKind::Context => inline.push(InlineEntry {
                    row: i,
                    side: Side::Right,
                }),
                RowKind::Removed => inline.push(InlineEntry {
                    row: i,
                    side: Side::Left,
                }),
                RowKind::Added => inline.push(InlineEntry {
                    row: i,
                    side: Side::Right,
                }),
                RowKind::Modified => {
                    inline.push(InlineEntry {
                        row: i,
                        side: Side::Left,
                    });
                    inline.push(InlineEntry {
                        row: i,
                        side: Side::Right,
                    });
                }
            }
        }

        let max_width = rows
            .iter()
            .map(|r| {
                let o = r.old_text.as_deref().map(line_width).unwrap_or(0);
                let n = r.new_text.as_deref().map(line_width).unwrap_or(0);
                o.max(n)
            })
            .max()
            .unwrap_or(0);

        FileDiff {
            rows,
            inline,
            additions,
            deletions,
            max_width,
        }
    }

    /// Number of visual lines in the given view mode.
    pub fn len(&self, side_by_side: bool) -> usize {
        if side_by_side {
            self.rows.len()
        } else {
            self.inline.len()
        }
    }

    /// Every run of context rows long enough to fold, as `[start, end)` row
    /// ranges — whether or not the user has expanded it.
    fn fold_ranges(&self) -> Vec<(usize, usize)> {
        let n = self.rows.len();
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            if self.rows[i].kind != RowKind::Context {
                i += 1;
                continue;
            }
            let s = i;
            while i < n && self.rows[i].kind == RowKind::Context {
                i += 1;
            }
            let e = i;
            // Keep a margin of context next to changes; none at file edges.
            let hs = s + if s == 0 { 0 } else { FOLD_MARGIN };
            let he = e.saturating_sub(if e == n { 0 } else { FOLD_MARGIN });
            if he > hs && he - hs >= FOLD_MIN {
                out.push((hs, he));
            }
        }
        out
    }

    /// The visible lines for the given view mode, folding long unchanged runs
    /// when `collapse` is on (minus any the user expanded).
    pub fn display(
        &self,
        side_by_side: bool,
        collapse: bool,
        expanded: &HashSet<usize>,
    ) -> Vec<DisplayEntry> {
        if !collapse {
            return (0..self.len(side_by_side))
                .map(DisplayEntry::Line)
                .collect();
        }
        let ranges = self.fold_ranges();
        let find = |row: usize| ranges.iter().find(|(s, e)| row >= *s && row < *e).copied();
        let mut out = Vec::new();
        let mut last_fold = None;
        let mut push = |row: usize, line: usize, out: &mut Vec<DisplayEntry>| match find(row) {
            Some((s, e)) => {
                // An expanded run keeps its lines but gains a click-to-fold
                // header, so the user can put it back where they opened it.
                if expanded.contains(&s) {
                    if last_fold != Some(s) {
                        out.push(DisplayEntry::Unfold {
                            start: s,
                            count: e - s,
                        });
                        last_fold = Some(s);
                    }
                    out.push(DisplayEntry::Line(line));
                } else if last_fold != Some(s) {
                    out.push(DisplayEntry::Fold {
                        start: s,
                        count: e - s,
                    });
                    last_fold = Some(s);
                }
            }
            None => out.push(DisplayEntry::Line(line)),
        };
        if side_by_side {
            for i in 0..self.rows.len() {
                push(i, i, &mut out);
            }
        } else {
            for (j, entry) in self.inline.iter().enumerate() {
                push(entry.row, j, &mut out);
            }
        }
        out
    }
}

/// A line-range selection on one side of the diff, in file line numbers.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub side: Side,
    pub anchor: usize,
    pub end: usize,
}

impl Selection {
    pub fn range(&self) -> (usize, usize) {
        if self.anchor <= self.end {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }
    pub fn contains(&self, side: Side, line: usize) -> bool {
        let (lo, hi) = self.range();
        self.side == side && line >= lo && line <= hi
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_modified_lines() {
        let d = FileDiff::compute(Some("a\nb\nc\n"), Some("a\nB\nc\n"));
        assert_eq!(d.rows.len(), 3);
        assert_eq!(d.rows[1].kind, RowKind::Modified);
        assert_eq!(d.rows[1].old_ln, Some(2));
        assert_eq!(d.rows[1].new_ln, Some(2));
        assert_eq!(d.inline.len(), 4); // context, old b, new B, context
        assert_eq!(d.additions, 1);
        assert_eq!(d.deletions, 1);
    }

    #[test]
    fn added_file() {
        let d = FileDiff::compute(None, Some("x\ny\n"));
        assert_eq!(d.rows.len(), 2);
        assert!(d.rows.iter().all(|r| r.kind == RowKind::Added));
        assert_eq!(d.rows[0].new_ln, Some(1));
    }

    #[test]
    fn removed_file() {
        let d = FileDiff::compute(Some("x\n"), None);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Removed);
    }

    #[test]
    fn folds_long_context_runs() {
        // 20 identical lines, one change in the middle.
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line10\n", "LINE10\n");
        let d = FileDiff::compute(Some(&old), Some(&new));
        let none = HashSet::new();
        let disp = d.display(true, true, &none);
        let folds: Vec<_> = disp
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::Fold { start, count } => Some((*start, *count)),
                _ => None,
            })
            .collect();
        // Head run: rows 0..9 context, keeps FOLD_MARGIN before the change,
        // hides 0..6. Tail run: rows 10..20 context, keeps margin after the
        // change, hides 13..20.
        assert_eq!(folds, vec![(0, 6), (13, 7)]);
        // Expanding the first fold reveals its rows again, and leaves a
        // click-to-fold header in place of the fold row.
        let mut open = HashSet::new();
        open.insert(0);
        let disp2 = d.display(true, true, &open);
        assert_eq!(
            disp2
                .iter()
                .filter(|e| matches!(e, DisplayEntry::Fold { .. }))
                .count(),
            1
        );
        assert_eq!(
            disp2
                .iter()
                .filter(|e| matches!(e, DisplayEntry::Unfold { start: 0, count: 6 }))
                .count(),
            1,
            "expanded run keeps a header so it can be folded again"
        );
        assert!(disp2.len() > disp.len());
        // Dropping it from `expanded` folds the run back exactly as before.
        assert_eq!(d.display(true, true, &HashSet::new()), disp);
        // Collapse off shows every row.
        assert_eq!(d.display(true, false, &none).len(), d.rows.len());
        // Inline view folds the same row ranges.
        let inl = d.display(false, true, &none);
        assert_eq!(
            inl.iter()
                .filter(|e| matches!(e, DisplayEntry::Fold { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn short_context_runs_stay_visible() {
        let d = FileDiff::compute(Some("a\nb\nc\nd\ne\n"), Some("A\nb\nc\nd\nE\n"));
        let none = HashSet::new();
        // Middle run of 3 context rows is under FOLD_MIN + margins: no folds.
        assert!(d
            .display(true, true, &none)
            .iter()
            .all(|e| matches!(e, DisplayEntry::Line(_))));
    }

    #[test]
    fn max_width_measures_widest_side_with_tabs() {
        let d = FileDiff::compute(Some("ab\n\tx\n"), Some("ab\nlonger line here\n"));
        // "\tx" is TAB_WIDTH + 1 = 5; the new side's 16 chars win.
        assert_eq!(d.max_width, 16);
        assert_eq!(line_width("\tx"), TAB_WIDTH + 1);
    }

    #[test]
    fn pure_addition_keeps_line_numbers() {
        let d = FileDiff::compute(Some("a\nc\n"), Some("a\nb\nc\n"));
        let added: Vec<_> = d.rows.iter().filter(|r| r.kind == RowKind::Added).collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].new_ln, Some(2));
    }
}
