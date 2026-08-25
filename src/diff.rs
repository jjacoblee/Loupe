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

/// Char index at display column `col`, expanding tabs the way the
/// renderer does — the inverse of [`line_width`] over a prefix, and what
/// turns a mouse click into a position in the text.
pub fn char_at_col(s: &str, col: usize) -> usize {
    let mut w = 0usize;
    for (i, ch) in s.chars().enumerate() {
        let cw = if ch == '\t' {
            TAB_WIDTH
        } else {
            ch.width().unwrap_or(0)
        };
        if w + cw > col {
            return i;
        }
        w += cw;
    }
    s.chars().count()
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

    /// Every run of changed rows, as `[start, end)` row ranges. One run is
    /// what the diff view calls a *section*: the unit `{` / `}` jump between,
    /// and the unit a revert puts back. Runs are maximal — a removal
    /// immediately followed by an addition is one section, because that is
    /// how it reads on screen.
    ///
    /// The renderer asks per row and uses [`Self::section_at`] instead; this
    /// is the whole-file view of the same model, which is what the tests
    /// reason about.
    #[cfg(test)]
    pub fn sections(&self) -> Vec<(usize, usize)> {
        let n = self.rows.len();
        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            if self.rows[i].kind == RowKind::Context {
                i += 1;
                continue;
            }
            let s = i;
            while i < n && self.rows[i].kind != RowKind::Context {
                i += 1;
            }
            out.push((s, i));
        }
        out
    }

    /// The section `row` belongs to, or None when it is a context row.
    /// Scans outwards from the row rather than walking the whole file —
    /// the renderer asks this once per visible line, every frame.
    pub fn section_at(&self, row: usize) -> Option<(usize, usize)> {
        if self.rows.get(row)?.kind == RowKind::Context {
            return None;
        }
        let mut s = row;
        while s > 0 && self.rows[s - 1].kind != RowKind::Context {
            s -= 1;
        }
        let mut e = row + 1;
        while e < self.rows.len() && self.rows[e].kind != RowKind::Context {
            e += 1;
        }
        Some((s, e))
    }

    /// Added and removed line counts within one section — what the confirm
    /// prompt says is at stake.
    pub fn section_counts(&self, (s, e): (usize, usize)) -> (usize, usize) {
        let mut adds = 0;
        let mut dels = 0;
        for row in self.rows.get(s..e).unwrap_or_default() {
            match row.kind {
                RowKind::Added => adds += 1,
                RowKind::Removed => dels += 1,
                RowKind::Modified => {
                    adds += 1;
                    dels += 1;
                }
                RowKind::Context => {}
            }
        }
        (adds, dels)
    }

    /// The new side of the file with one section put back the way it was:
    /// every other line stays exactly as it is now, and inside `[s, e)` the
    /// old text replaces the new.
    ///
    /// Rebuilt from the row model rather than spliced by line number,
    /// because the rows already hold every line of both sides — so what is
    /// written can never drift out of step with what is on screen.
    ///
    /// `None` means the file should no longer exist: reverting the whole of
    /// a file the change created leaves nothing to write.
    pub fn revert_section(
        &self,
        (s, e): (usize, usize),
        old: Option<&str>,
        new: Option<&str>,
    ) -> Option<String> {
        let mut out = String::new();
        let mut wrote = false;
        // Which side the last line came from decides whether the result
        // ends in a newline — the row texts have theirs stripped.
        let mut last_old = false;
        for (i, row) in self.rows.iter().enumerate() {
            let inside = i >= s && i < e;
            let text = if inside {
                row.old_text.as_deref()
            } else {
                row.new_text.as_deref()
            };
            let Some(t) = text else { continue };
            if wrote {
                out.push('\n');
            }
            out.push_str(t);
            wrote = true;
            last_old = inside;
        }
        if !wrote {
            // Nothing left at all: a file the change added, reverted whole.
            // An empty file that existed before is still a file.
            return old.map(|_| String::new());
        }
        let source = if last_old { old } else { new };
        if source.is_some_and(|c| c.ends_with('\n')) {
            out.push('\n');
        }
        Some(out)
    }

    /// The fold that hides `row`, keyed by its start row — what a jump has
    /// to expand before it can land there.
    pub fn fold_start_for(&self, row: usize) -> Option<usize> {
        self.fold_ranges()
            .into_iter()
            .find(|(s, e)| row >= *s && row < *e)
            .map(|(s, _)| s)
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

/// A position in one side of the diff: a 1-based file line, and a char
/// index within that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Pos { line, col }
    }
}

/// A selection on one side of the diff.
///
/// Two things want a selection and they want different granularities:
/// review comments anchor to whole lines (GitHub has nowhere to put a
/// half-line comment), while copying wants exactly the characters the
/// pointer went over. So the selection carries character positions and a
/// `linewise` flag — clicking a line or using `V` sets it, dragging
/// through text clears it — and each caller reads the part it needs.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub side: Side,
    pub anchor: Pos,
    pub end: Pos,
    /// Whole lines, however the columns happen to fall.
    pub linewise: bool,
}

impl Selection {
    /// A whole-line selection, the shape `V` and a plain click produce.
    pub fn lines(side: Side, anchor: usize, end: usize) -> Self {
        Selection {
            side,
            anchor: Pos::new(anchor, 0),
            end: Pos::new(end, 0),
            linewise: true,
        }
    }

    /// The two ends in document order.
    pub fn ordered(&self) -> (Pos, Pos) {
        if self.anchor <= self.end {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// The line span, which is what commenting anchors to.
    pub fn range(&self) -> (usize, usize) {
        let (lo, hi) = self.ordered();
        (lo.line, hi.line)
    }

    pub fn contains(&self, side: Side, line: usize) -> bool {
        let (lo, hi) = self.range();
        self.side == side && line >= lo && line <= hi
    }

    /// The selected char range within one line, given how long that line
    /// is — `None` when the line is outside the selection. This is what
    /// the renderer paints and what the clipboard copies, so the two can
    /// never disagree about what "selected" means.
    pub fn cols_on(&self, side: Side, line: usize, len: usize) -> Option<(usize, usize)> {
        if !self.contains(side, line) {
            return None;
        }
        if self.linewise {
            return Some((0, len));
        }
        let (lo, hi) = self.ordered();
        let start = if line == lo.line { lo.col } else { 0 };
        let end = if line == hi.line { hi.col } else { len };
        Some((start.min(len), end.min(len)))
    }

    /// The selected text, taken from the lines of the side it is on.
    /// Empty when the selection covers no characters at all.
    pub fn text(&self, content: &str) -> String {
        let (lo, hi) = self.range();
        let mut out: Vec<String> = Vec::new();
        for (i, text) in content
            .lines()
            .enumerate()
            .skip(lo.saturating_sub(1))
            .take(hi + 1 - lo)
        {
            let line = i + 1;
            let chars: Vec<char> = text.chars().collect();
            match self.cols_on(self.side, line, chars.len()) {
                Some((a, b)) if b > a => out.push(chars[a..b].iter().collect()),
                Some(_) => out.push(String::new()),
                None => {}
            }
        }
        out.join("\n")
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
    fn sections_group_adjacent_changed_rows() {
        // Two runs of changes, far enough apart to stay separate; the
        // removal + addition in the middle is ONE section, not two.
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nd\ne\nGONE\nADDED\ng\n";
        let d = FileDiff::compute(Some(old), Some(new));
        let sections = d.sections();
        assert_eq!(sections.len(), 2, "{:?}", d.rows);
        // Every row of a section is a changed row, and the rows either side
        // of it are context.
        for (s, e) in sections.iter().copied() {
            assert!(d.rows[s..e].iter().all(|r| r.kind != RowKind::Context));
            assert_eq!(d.rows[s - 1].kind, RowKind::Context);
            assert_eq!(d.rows[e].kind, RowKind::Context);
            // Any row inside it finds the same section back.
            for row in s..e {
                assert_eq!(d.section_at(row), Some((s, e)));
            }
        }
        // A context row belongs to no section — there is nothing to revert.
        assert_eq!(d.section_at(0), None);
        assert_eq!(d.section_counts(sections[0]), (1, 1));
    }

    #[test]
    fn reverting_a_section_puts_back_only_that_section() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nB\nc\nd\ne\nF1\nF2\ng\n";
        let d = FileDiff::compute(Some(old), Some(new));
        let sections = d.sections();
        assert_eq!(sections.len(), 2);

        // The first section back to the old text; the second one untouched.
        assert_eq!(
            d.revert_section(sections[0], Some(old), Some(new)).unwrap(),
            "a\nb\nc\nd\ne\nF1\nF2\ng\n"
        );
        // …and the other way round.
        assert_eq!(
            d.revert_section(sections[1], Some(old), Some(new)).unwrap(),
            "a\nB\nc\nd\ne\nf\ng\n"
        );
        // Reverting every section in turn arrives back at the old file.
        let mut content = new.to_string();
        for _ in 0..sections.len() {
            let d = FileDiff::compute(Some(old), Some(&content));
            let first = d.sections()[0];
            content = d.revert_section(first, Some(old), Some(&content)).unwrap();
        }
        assert_eq!(content, old);
    }

    #[test]
    fn reverting_keeps_the_files_own_line_endings() {
        // No trailing newline on the new side: the rebuilt file must not
        // grow one.
        let d = FileDiff::compute(Some("a\nb\n"), Some("a\nB"));
        let s = d.sections()[0];
        assert_eq!(
            d.revert_section(s, Some("a\nb\n"), Some("a\nB")).unwrap(),
            "a\nb\n",
            "the reverted last line brings the old side's newline with it"
        );
        // CRLF survives the round trip.
        let d = FileDiff::compute(Some("a\r\nb\r\n"), Some("a\r\nB\r\n"));
        let s = d.sections()[0];
        assert_eq!(
            d.revert_section(s, Some("a\r\nb\r\n"), Some("a\r\nB\r\n"))
                .unwrap(),
            "a\r\nb\r\n"
        );
    }

    #[test]
    fn reverting_a_whole_new_file_leaves_nothing_to_write() {
        let d = FileDiff::compute(None, Some("x\ny\n"));
        let s = d.sections()[0];
        assert_eq!(s, (0, 2), "an added file is one section");
        assert_eq!(
            d.revert_section(s, None, Some("x\ny\n")),
            None,
            "there is no old file to go back to — it should be deleted"
        );
        // A file that was emptied rather than created still exists.
        let d = FileDiff::compute(Some(""), Some("x\n"));
        let s = d.sections()[0];
        assert_eq!(
            d.revert_section(s, Some(""), Some("x\n")),
            Some(String::new())
        );
        // A deleted file comes back whole.
        let d = FileDiff::compute(Some("x\ny\n"), None);
        let s = d.sections()[0];
        assert_eq!(
            d.revert_section(s, Some("x\ny\n"), None),
            Some("x\ny\n".into())
        );
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

    #[test]
    fn a_line_selection_covers_whole_lines() {
        let sel = Selection::lines(Side::Right, 2, 3);
        assert_eq!(sel.range(), (2, 3));
        assert!(sel.contains(Side::Right, 2));
        assert!(!sel.contains(Side::Left, 2));
        assert_eq!(sel.cols_on(Side::Right, 2, 7), Some((0, 7)));
        assert_eq!(sel.cols_on(Side::Right, 4, 7), None);
        assert_eq!(sel.text("aaa\nbbb\nccc\nddd\n"), "bbb\nccc");
    }

    #[test]
    fn a_character_selection_covers_exactly_what_was_dragged() {
        let content = "const alpha = 1;\nconst beta = 2;\nconst gamma = 3;\n";
        // From "alpha" on line 1 through "beta" on line 2.
        let sel = Selection {
            side: Side::Left,
            anchor: Pos::new(1, 6),
            end: Pos::new(2, 10),
            linewise: false,
        };
        assert_eq!(sel.cols_on(Side::Left, 1, 16), Some((6, 16)));
        assert_eq!(sel.cols_on(Side::Left, 2, 15), Some((0, 10)));
        assert_eq!(sel.text(content), "alpha = 1;\nconst beta");

        // Dragged the other way: the same text, not an empty selection.
        let backwards = Selection {
            anchor: sel.end,
            end: sel.anchor,
            ..sel
        };
        assert_eq!(backwards.text(content), sel.text(content));

        // Within one line.
        let one = Selection {
            side: Side::Left,
            anchor: Pos::new(2, 6),
            end: Pos::new(2, 10),
            linewise: false,
        };
        assert_eq!(one.text(content), "beta");

        // A column past the end of a shorter line clamps instead of
        // panicking — dragging down a ragged block does this constantly.
        let ragged = Selection {
            side: Side::Left,
            anchor: Pos::new(1, 200),
            end: Pos::new(3, 200),
            linewise: false,
        };
        assert_eq!(ragged.cols_on(Side::Left, 1, 16), Some((16, 16)));
        assert_eq!(ragged.text(content), "\nconst beta = 2;\nconst gamma = 3;");

        // The side matters: this selection is on the old side, so the new
        // side shows nothing highlighted.
        assert_eq!(sel.cols_on(Side::Right, 1, 16), None);
    }
}
