//! Line-diff engine: builds an aligned row model that serves both the
//! side-by-side view and the inline (stacked) view.

use similar::{ChangeTag, TextDiff, TextDiffConfig};
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

/// Longest line the word diff will look inside.
///
/// A minified bundle arrives as one line of a hundred thousand
/// characters, and word-diffing two of those costs more than the rest of
/// the file put together. Past this a row keeps the plain red and green
/// it always had.
const MAX_WORD_DIFF: usize = 4_000;

/// Past this share of a line, the word highlight stops saying anything.
///
/// Two lines that share almost nothing are a rewrite, and painting nine
/// tenths of both of them darker is louder than painting neither.
const WORD_DIFF_MAX_SHARE: f32 = 0.7;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowKind {
    #[default]
    Context,
    Added,
    Removed,
    /// A removed line paired with an added line (rendered on one screen row
    /// in side-by-side mode, two stacked rows in inline mode).
    Modified,
}

/// Which of a stacked diff's three layers changed a row.
///
/// An ordinary two-way diff has one layer and every row is
/// [`Layer::Local`]. A stacked diff ([`FileDiff::stack`]) reads three
/// versions of the file — the base branch, the branch as the remote has
/// it, and the working tree — and says of each row which of the two
/// steps between them produced it.
///
/// The one it exists for is [`Layer::Rework`]: a line the pushed branch
/// already rewrote and the working tree is rewriting again. Nothing in a
/// two-way diff can tell that apart from new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    /// The working tree changed it and the pushed branch did not. New
    /// work, not on the remote yet.
    #[default]
    Local,
    /// The pushed branch changed it and the working tree has not touched
    /// it since. Already in the pull request.
    Pushed,
    /// Both changed it.
    Rework,
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    pub kind: RowKind,
    pub old_ln: Option<usize>,
    pub new_ln: Option<usize>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    /// Character ranges the two versions of this line do not share, one
    /// list per side. Empty on every row but a [`RowKind::Modified`] one,
    /// which is the only kind with a counterpart to differ from.
    ///
    /// The renderer paints these darker than the rest of the line. A row
    /// painted whole says the line changed and nothing about where, and
    /// on a long line with one renamed variable in it that is two lines
    /// the reader has to compare character by character.
    pub old_words: Ranges,
    pub new_words: Ranges,
    /// Which layer of a stacked diff changed this row. Always
    /// [`Layer::Local`] in an ordinary two-way diff, which has one layer.
    pub layer: Layer,
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
    /// Rows carrying [`Layer::Rework`] — lines this change has now
    /// rewritten twice. Zero in an ordinary two-way diff.
    pub reworked: usize,
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

/// Character ranges within one line — what the renderer measures every
/// overlay in, and what the word highlight is a list of.
pub type Ranges = Vec<(usize, usize)>;

/// One token of a line: where it sits in bytes, to slice the text, and
/// in characters, which is what the renderer measures overlays in.
struct Tok {
    bytes: (usize, usize),
    chars: (usize, usize),
}

/// Split a line into the pieces a reader compares.
///
/// A run of identifier characters is one token, a run of whitespace is
/// one token, and every other character stands alone — so a changed
/// bracket reads as a changed bracket rather than as a changed
/// expression.
///
/// `similar`'s own word splitter breaks on whitespace and nothing else,
/// which makes `Client::builder().timeout(timeout)` a single word. One
/// renamed argument inside it then reads as the whole expression
/// changing, and a highlight that covers the whole expression says no
/// more than the red and green already did.
fn tokenize(line: &str) -> Vec<Tok> {
    let chars: Vec<char> = line.chars().collect();
    let class = |ch: char| {
        if ch.is_alphanumeric() || ch == '_' {
            0u8
        } else if ch.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut b = 0usize;
    while i < chars.len() {
        let k = class(chars[i]);
        let (start_c, start_b) = (i, b);
        loop {
            b += chars[i].len_utf8();
            i += 1;
            if k == 2 || i >= chars.len() || class(chars[i]) != k {
                break;
            }
        }
        out.push(Tok {
            bytes: (start_b, b),
            chars: (start_c, i),
        });
    }
    out
}

/// The character ranges two versions of one line do not share.
///
/// Tokens rather than characters: a renamed variable reads as one range
/// instead of a scatter of letters, and a scatter is exactly what the eye
/// cannot pick out of a line that is already painted whole.
///
/// Returns two empty lists when the highlight would say nothing — a line
/// too long to be worth the work, or two lines that share so little they
/// are a rewrite rather than an edit.
fn changed_words(old: &str, new: &str) -> (Ranges, Ranges) {
    if old.len() > MAX_WORD_DIFF || new.len() > MAX_WORD_DIFF {
        return (Vec::new(), Vec::new());
    }
    let (ot, nt) = (tokenize(old), tokenize(new));
    let ow: Vec<&str> = ot.iter().map(|t| &old[t.bytes.0..t.bytes.1]).collect();
    let nw: Vec<&str> = nt.iter().map(|t| &new[t.bytes.0..t.bytes.1]).collect();
    let mut olds: Ranges = Vec::new();
    let mut news: Ranges = Vec::new();
    for change in TextDiff::from_slices(&ow, &nw).iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {}
            ChangeTag::Delete => {
                if let Some(i) = change.old_index() {
                    olds.push(ot[i].chars);
                }
            }
            ChangeTag::Insert => {
                if let Some(i) = change.new_index() {
                    news.push(nt[i].chars);
                }
            }
        }
    }
    join_touching(&mut olds);
    join_touching(&mut news);
    let share = |spans: &[(usize, usize)], len: usize| {
        if len == 0 {
            return 0.0;
        }
        spans.iter().map(|(s, e)| e - s).sum::<usize>() as f32 / len as f32
    };
    if share(&olds, old.chars().count()) > WORD_DIFF_MAX_SHARE
        || share(&news, new.chars().count()) > WORD_DIFF_MAX_SHARE
    {
        return (Vec::new(), Vec::new());
    }
    (olds, news)
}

/// Join ranges that touch, so a changed word beside changed punctuation
/// is one highlight rather than three with nothing between them.
fn join_touching(spans: &mut Ranges) {
    let mut out: Ranges = Vec::with_capacity(spans.len());
    for (s, e) in spans.drain(..) {
        match out.last_mut() {
            Some(last) if last.1 >= s => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    *spans = out;
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

        let flush = |rows: &mut Vec<Row>,
                     dels: &mut Vec<(usize, String)>,
                     inss: &mut Vec<(usize, String)>| {
            let paired = dels.len().min(inss.len());
            for i in 0..paired {
                // The one kind of row with a counterpart to differ from,
                // and so the only one where "what changed" is a question
                // narrower than "this whole line".
                let (old_words, new_words) = changed_words(&dels[i].1, &inss[i].1);
                rows.push(Row {
                    kind: RowKind::Modified,
                    old_ln: Some(dels[i].0),
                    new_ln: Some(inss[i].0),
                    old_text: Some(dels[i].1.clone()),
                    new_text: Some(inss[i].1.clone()),
                    old_words,
                    new_words,
                    layer: Layer::Local,
                });
            }
            for d in dels.iter().skip(paired) {
                rows.push(Row {
                    kind: RowKind::Removed,
                    old_ln: Some(d.0),
                    new_ln: None,
                    old_text: Some(d.1.clone()),
                    new_text: None,
                    ..Row::default()
                });
            }
            for a in inss.iter().skip(paired) {
                rows.push(Row {
                    kind: RowKind::Added,
                    old_ln: None,
                    new_ln: Some(a.0),
                    old_text: None,
                    new_text: Some(a.1.clone()),
                    ..Row::default()
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
                        ..Row::default()
                    });
                }
                ChangeTag::Delete => dels.push((change.old_index().unwrap_or(0) + 1, text)),
                ChangeTag::Insert => inss.push((change.new_index().unwrap_or(0) + 1, text)),
            }
        }
        flush(&mut rows, &mut dels, &mut inss);

        // Degenerate case: both sides empty -> no rows at all.
        if old.is_none() && new.is_none() {
            rows.clear();
        }

        Self::assemble(rows)
    }

    /// Everything a [`FileDiff`] holds beyond its rows: the inline view,
    /// the counts, and the width to scroll to.
    ///
    /// Both the two-way [`Self::compute`] and the stacked [`Self::stack`]
    /// end here, so a stacked diff scrolls, folds and counts exactly the
    /// way an ordinary one does.
    fn assemble(rows: Vec<Row>) -> Self {
        let mut inline = Vec::with_capacity(rows.len() * 2);
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

        let additions = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Added | RowKind::Modified))
            .count();
        let deletions = rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Removed | RowKind::Modified))
            .count();
        let reworked = rows
            .iter()
            .filter(|r| r.kind != RowKind::Context && r.layer == Layer::Rework)
            .count();

        FileDiff {
            rows,
            inline,
            additions,
            deletions,
            max_width,
            reworked,
        }
    }

    /// The whole stack of a change, as one before-and-after: `base` on the
    /// left, the working tree on the right, and every row labelled with
    /// the layer that changed it.
    ///
    /// `pushed` is the middle version — the branch as the remote has it.
    /// It never appears on screen; it is the spine the two steps of the
    /// change are measured against. A row whose line the pushed branch
    /// changed is [`Layer::Pushed`], one only the working tree changed is
    /// [`Layer::Local`], and one both changed is [`Layer::Rework`].
    ///
    /// Both steps are diffed against that same middle version, so their
    /// rows line up on the pushed line number and the merge is exact —
    /// no third diff, and no guessing which change a row came from.
    ///
    /// It costs two line diffs rather than one, and the caller two more
    /// `git show` calls to read the versions. That is why the setting is
    /// off by default.
    ///
    /// Two cases have nothing to draw and so are absent from the result,
    /// as they are from any base-to-working-tree diff: a line the pushed
    /// branch added and the working tree deleted, and one the pushed
    /// branch changed and the working tree changed back to what the base
    /// said.
    pub fn stack(base: Option<&str>, pushed: Option<&str>, local: Option<&str>) -> Self {
        let up = Self::compute(base, pushed);
        let down = Self::compute(pushed, local);

        // Length of the middle version, taken from whichever step saw the
        // last of its lines.
        let n = up
            .rows
            .iter()
            .filter_map(|r| r.new_ln)
            .chain(down.rows.iter().filter_map(|r| r.old_ln))
            .max()
            .unwrap_or(0);

        // What the base had at each line of the middle version, and
        // whether the two differ. `None` where the pushed branch added
        // the line, so the base has nothing there.
        let mut was: Vec<Option<(usize, String)>> = vec![None; n];
        let mut pushed_changed = vec![false; n];
        // Base lines the pushed branch dropped, each keyed by the middle
        // line it sits in front of.
        let mut dropped: Vec<(usize, usize, String)> = Vec::new();
        let mut at = 0usize;
        for row in &up.rows {
            match row.new_ln {
                Some(p) => {
                    at = p;
                    was[p - 1] = row
                        .old_ln
                        .map(|b| (b, row.old_text.clone().unwrap_or_default()));
                    pushed_changed[p - 1] = row.kind != RowKind::Context;
                }
                None => {
                    if let (Some(b), Some(t)) = (row.old_ln, row.old_text.as_ref()) {
                        dropped.push((at + 1, b, t.clone()));
                    }
                }
            }
        }

        // The same read of the second step: what the working tree has at
        // each line of the middle version, and what it added between them.
        let mut now: Vec<Option<(usize, String)>> = vec![None; n];
        let mut local_changed = vec![false; n];
        let mut inserted: Vec<(usize, usize, String)> = Vec::new();
        let mut at = 0usize;
        for row in &down.rows {
            match row.old_ln {
                Some(p) => {
                    at = p;
                    now[p - 1] = row
                        .new_ln
                        .map(|l| (l, row.new_text.clone().unwrap_or_default()));
                    local_changed[p - 1] = row.kind != RowKind::Context;
                }
                None => {
                    if let (Some(l), Some(t)) = (row.new_ln, row.new_text.as_ref()) {
                        inserted.push((at + 1, l, t.clone()));
                    }
                }
            }
        }

        let mut rows: Vec<Row> = Vec::with_capacity(n + dropped.len() + inserted.len());
        let (mut di, mut ii) = (0usize, 0usize);
        // One pass down the middle version. Anything anchored in front of
        // a line goes first, in the order it reads: what the base lost,
        // then what the working tree gained. The extra step past `n`
        // flushes whatever is anchored at the end of the file.
        for p in 1..=n + 1 {
            while di < dropped.len() && dropped[di].0 <= p {
                let (_, b, t) = &dropped[di];
                rows.push(Row {
                    kind: RowKind::Removed,
                    old_ln: Some(*b),
                    old_text: Some(t.clone()),
                    layer: Layer::Pushed,
                    ..Row::default()
                });
                di += 1;
            }
            while ii < inserted.len() && inserted[ii].0 <= p {
                let (_, l, t) = &inserted[ii];
                rows.push(Row {
                    kind: RowKind::Added,
                    new_ln: Some(*l),
                    new_text: Some(t.clone()),
                    layer: Layer::Local,
                    ..Row::default()
                });
                ii += 1;
            }
            if p > n {
                break;
            }
            let layer = match (pushed_changed[p - 1], local_changed[p - 1]) {
                (true, true) => Layer::Rework,
                (true, false) => Layer::Pushed,
                _ => Layer::Local,
            };
            match (was[p - 1].take(), now[p - 1].take()) {
                // Added by the push and deleted since: in neither of the
                // two versions on screen.
                (None, None) => {}
                (Some((b, old)), None) => rows.push(Row {
                    kind: RowKind::Removed,
                    old_ln: Some(b),
                    old_text: Some(old),
                    layer,
                    ..Row::default()
                }),
                (None, Some((l, new))) => rows.push(Row {
                    kind: RowKind::Added,
                    new_ln: Some(l),
                    new_text: Some(new),
                    layer,
                    ..Row::default()
                }),
                (Some((b, old)), Some((l, new))) if old == new => rows.push(Row {
                    kind: RowKind::Context,
                    old_ln: Some(b),
                    new_ln: Some(l),
                    old_text: Some(old),
                    new_text: Some(new),
                    ..Row::default()
                }),
                (Some((b, old)), Some((l, new))) => {
                    let (old_words, new_words) = changed_words(&old, &new);
                    rows.push(Row {
                        kind: RowKind::Modified,
                        old_ln: Some(b),
                        new_ln: Some(l),
                        old_text: Some(old),
                        new_text: Some(new),
                        old_words,
                        new_words,
                        layer,
                    });
                }
            }
        }
        Self::assemble(rows)
    }

    /// Only what the working tree changed since the push — `pushed` on
    /// the left, the working tree on the right — with every row that
    /// lands on a line the pushed branch had already changed marked
    /// [`Layer::Rework`].
    ///
    /// The quiet half of [`Self::stack`]: the same question about the
    /// same three versions, without the pull request's own changes on
    /// screen. A line you added since the push is new work and reads as
    /// [`Layer::Local`], because it has no counterpart in the pushed
    /// version for the pull request to have changed; the stacked view is
    /// where you see whether it landed inside a block the pull request
    /// had already rewritten.
    pub fn since_push(base: Option<&str>, pushed: Option<&str>, local: Option<&str>) -> Self {
        let up = Self::compute(base, pushed);
        let n = up.rows.iter().filter_map(|r| r.new_ln).max().unwrap_or(0);
        let mut pushed_changed = vec![false; n];
        for row in &up.rows {
            if let Some(p) = row.new_ln {
                pushed_changed[p - 1] = row.kind != RowKind::Context;
            }
        }
        let mut d = Self::compute(pushed, local);
        for row in &mut d.rows {
            if row.kind == RowKind::Context {
                continue;
            }
            let again = row
                .old_ln
                .is_some_and(|p| pushed_changed.get(p - 1).copied().unwrap_or(false));
            row.layer = if again { Layer::Rework } else { Layer::Local };
        }
        d.reworked = d
            .rows
            .iter()
            .filter(|r| r.kind != RowKind::Context && r.layer == Layer::Rework)
            .count();
        d
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

    // --------------------------------------------------- the stacked diff

    /// base → pushed → local, with one line changed by each step and one
    /// changed by both.
    const BASE: &str = "one\ntwo\nthree\nfour\n";
    const PUSHED: &str = "one\nTWO\nthree\nFOUR\n";
    const LOCAL: &str = "one\nTWO\nTHREE\nfour again\n";

    #[test]
    fn stack_labels_each_layer() {
        let d = FileDiff::stack(Some(BASE), Some(PUSHED), Some(LOCAL));
        assert_eq!(d.rows.len(), 4);
        // "one" is untouched by both steps.
        assert_eq!(d.rows[0].kind, RowKind::Context);
        // "two" → "TWO" is the push's, and the working tree left it alone.
        assert_eq!(d.rows[1].kind, RowKind::Modified);
        assert_eq!(d.rows[1].layer, Layer::Pushed);
        assert_eq!(d.rows[1].old_text.as_deref(), Some("two"));
        assert_eq!(d.rows[1].new_text.as_deref(), Some("TWO"));
        // "three" → "THREE" is the working tree's alone.
        assert_eq!(d.rows[2].layer, Layer::Local);
        // "four" was rewritten by the push and again since.
        assert_eq!(d.rows[3].layer, Layer::Rework);
        assert_eq!(d.rows[3].old_text.as_deref(), Some("four"));
        assert_eq!(d.rows[3].new_text.as_deref(), Some("four again"));
        assert_eq!(d.reworked, 1);
    }

    #[test]
    fn stack_left_side_is_the_base() {
        // Every row reads base on the left and working tree on the right,
        // so the stacked view is still the diff a reviewer would see.
        let d = FileDiff::stack(Some(BASE), Some(PUSHED), Some(LOCAL));
        let left: Vec<_> = d
            .rows
            .iter()
            .filter_map(|r| r.old_text.as_deref())
            .collect();
        let right: Vec<_> = d
            .rows
            .iter()
            .filter_map(|r| r.new_text.as_deref())
            .collect();
        assert_eq!(left, vec!["one", "two", "three", "four"]);
        assert_eq!(right, vec!["one", "TWO", "THREE", "four again"]);
    }

    #[test]
    fn stack_matches_a_plain_diff_of_the_two_ends() {
        // The rows are the same ones a two-way diff of base and working
        // tree gives; only the labels are new.
        let flat = FileDiff::compute(Some(BASE), Some(LOCAL));
        let d = FileDiff::stack(Some(BASE), Some(PUSHED), Some(LOCAL));
        assert_eq!(d.additions, flat.additions);
        assert_eq!(d.deletions, flat.deletions);
        let kinds: Vec<_> = d.rows.iter().map(|r| r.kind).collect();
        let flat_kinds: Vec<_> = flat.rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, flat_kinds);
    }

    #[test]
    fn stack_marks_a_line_the_push_added_and_the_tree_changed() {
        let d = FileDiff::stack(Some("a\n"), Some("a\nb\n"), Some("a\nB\n"));
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[1].kind, RowKind::Added);
        assert_eq!(d.rows[1].new_text.as_deref(), Some("B"));
        assert_eq!(d.rows[1].layer, Layer::Rework);
    }

    #[test]
    fn stack_hides_a_line_the_push_added_and_the_tree_deleted() {
        // Neither end of the stack has it, so there is nothing to draw.
        let d = FileDiff::stack(Some("a\n"), Some("a\nb\n"), Some("a\n"));
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Context);
        assert_eq!(d.reworked, 0);
    }

    #[test]
    fn stack_keeps_what_the_push_removed() {
        let d = FileDiff::stack(Some("a\nb\n"), Some("a\n"), Some("a\n"));
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[1].kind, RowKind::Removed);
        assert_eq!(d.rows[1].layer, Layer::Pushed);
    }

    #[test]
    fn stack_keeps_what_the_tree_added() {
        let d = FileDiff::stack(Some("a\n"), Some("a\n"), Some("a\nz\n"));
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.rows[1].kind, RowKind::Added);
        assert_eq!(d.rows[1].layer, Layer::Local);
    }

    #[test]
    fn since_push_shows_only_the_second_step() {
        let d = FileDiff::since_push(Some(BASE), Some(PUSHED), Some(LOCAL));
        // The push's own change to "two" is not a change here.
        let changed: Vec<_> = d
            .rows
            .iter()
            .filter(|r| r.kind != RowKind::Context)
            .map(|r| (r.new_text.as_deref(), r.layer))
            .collect();
        assert_eq!(
            changed,
            vec![
                (Some("THREE"), Layer::Local),
                (Some("four again"), Layer::Rework),
            ]
        );
        assert_eq!(d.reworked, 1);
    }

    #[test]
    fn since_push_with_nothing_pushed_yet() {
        // No second step: every row is context and nothing is rework.
        let d = FileDiff::since_push(Some(BASE), Some(PUSHED), Some(PUSHED));
        assert!(d.rows.iter().all(|r| r.kind == RowKind::Context));
        assert_eq!(d.reworked, 0);
    }

    #[test]
    fn stack_of_an_added_file() {
        let d = FileDiff::stack(None, Some("x\n"), Some("X\n"));
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].kind, RowKind::Added);
        assert_eq!(d.rows[0].layer, Layer::Rework);
    }

    #[test]
    fn stack_without_a_push_reads_as_all_local() {
        // Nothing is on the remote yet, so the middle version is the base
        // and every change belongs to the working tree.
        let d = FileDiff::stack(Some(BASE), Some(BASE), Some(LOCAL));
        assert!(d
            .rows
            .iter()
            .filter(|r| r.kind != RowKind::Context)
            .all(|r| r.layer == Layer::Local));
        assert_eq!(d.reworked, 0);
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

    // ------------------------------------------------- changed words

    /// The text a set of ranges covers, joined by `|`, so a test can say
    /// what it expects to be painted rather than count characters.
    fn covered(line: &str, spans: &[(usize, usize)]) -> String {
        let chars: Vec<char> = line.chars().collect();
        spans
            .iter()
            .map(|(s, e)| chars[*s..*e].iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("|")
    }

    /// The case the whole feature exists for: one renamed name in a line
    /// that is otherwise identical. Both sides say which word, not just
    /// that the line moved.
    #[test]
    fn a_renamed_name_is_the_only_thing_highlighted() {
        let (old, new) = changed_words(
            "fn handle(request: Request, timeout: Duration) -> Result<Response> {",
            "fn handle(request: Request, deadline: Duration) -> Result<Response> {",
        );
        assert_eq!(
            covered(
                "fn handle(request: Request, timeout: Duration) -> Result<Response> {",
                &old
            ),
            "timeout"
        );
        assert_eq!(
            covered(
                "fn handle(request: Request, deadline: Duration) -> Result<Response> {",
                &new
            ),
            "deadline"
        );
    }

    /// Punctuation is its own token, so a name buried in an expression
    /// still reads as one name. `similar`'s own word splitter breaks on
    /// whitespace alone and would call this whole chain one word.
    #[test]
    fn a_name_inside_an_expression_is_found_on_its_own() {
        let o = "    let client = Client::builder().timeout(timeout).build()?;";
        let n = "    let client = Client::builder().timeout(deadline).build()?;";
        let (old, new) = changed_words(o, n);
        assert_eq!(covered(o, &old), "timeout");
        assert_eq!(covered(n, &new), "deadline");
    }

    /// A change in whitespace is a change. It is also the one a reader
    /// has no chance of seeing without this.
    #[test]
    fn a_changed_space_is_visible() {
        let o = "let x = 1;";
        let n = "let  x = 1;";
        let (_, new) = changed_words(o, n);
        assert_eq!(covered(n, &new), "  ", "the wider gap is marked");
    }

    /// Two lines that share almost nothing are a rewrite. Painting nine
    /// tenths of both of them darker says less than painting neither.
    #[test]
    fn a_rewritten_line_keeps_its_plain_colours() {
        let (old, new) = changed_words(
            "let total = items.iter().map(|i| i.price).sum();",
            "eprintln!(\"done\");",
        );
        assert!(old.is_empty() && new.is_empty());
    }

    /// A line long enough to be generated is not worth the work, and
    /// word-diffing two of them costs more than the rest of the file.
    #[test]
    fn a_very_long_line_is_left_alone() {
        let o = "x".repeat(MAX_WORD_DIFF + 1);
        let n = format!("{}y", "x".repeat(MAX_WORD_DIFF));
        let (old, new) = changed_words(&o, &n);
        assert!(old.is_empty() && new.is_empty());
    }

    /// Only a paired row has a counterpart to differ from. A whole added
    /// or removed line is all change, and a second shade on it would
    /// mean nothing.
    #[test]
    fn only_paired_rows_carry_word_ranges() {
        let d = FileDiff::compute(Some("one\ntwo\n"), Some("one\ntwo two\nthree\n"));
        let modified = d
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Modified)
            .expect("the edited line pairs up");
        assert!(!modified.new_words.is_empty(), "and says which words moved");
        for row in d.rows.iter().filter(|r| r.kind != RowKind::Modified) {
            assert!(row.old_words.is_empty() && row.new_words.is_empty());
        }
    }

    /// Ranges that touch are one highlight. Three of them with nothing
    /// between reads as three changes.
    #[test]
    fn touching_ranges_join_up() {
        let mut spans = vec![(0, 3), (3, 5), (7, 9)];
        join_touching(&mut spans);
        assert_eq!(spans, vec![(0, 5), (7, 9)]);
    }
}
