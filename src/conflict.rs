//! Merge conflict markers: read them, show them, and write the result.
//!
//! Git writes a conflicted file as runs of alternatives between marker
//! lines. This module turns those runs into a list of [`Hunk`] values, and
//! turns the list back into file text once the reader picks a side.
//!
//! The two sides also become two plain files (see [`Conflicted::sides`]).
//! Every agreed line appears in both, and every conflict hunk appears in
//! one or the other. The diff engine then aligns them, so the review pane
//! shows a merge conflict with the same rows, folds, colors, and search
//! that it shows a normal change with. The marker lines never reach the
//! screen.

use std::collections::HashMap;

/// The four marker prefixes git writes. A marker line is exactly 7 of the
/// character, then either the end of the line or a space and a label.
const OURS: &str = "<<<<<<<";
const BASE: &str = "|||||||";
const SEP: &str = "=======";
const THEIRS: &str = ">>>>>>>";

/// True when `line` is the marker `m`, and not code that starts the same
/// way. Git demands the 7 characters stand alone or carry a label.
fn is_marker(line: &str, m: &str) -> bool {
    line.strip_prefix(m)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// The label after a marker, with the marker and one space removed.
fn label_of(line: &str, m: &str) -> String {
    line.strip_prefix(m)
        .map(|rest| rest.trim_start().to_string())
        .unwrap_or_default()
}

/// Which side of a conflict hunk to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The current branch — what `<<<<<<<` opens.
    Ours,
    /// The incoming branch — what `>>>>>>>` closes.
    Theirs,
    /// Our lines, then their lines. The order the file already has them in.
    Both,
    /// The common ancestor. Only offered when git wrote one (`diff3` and
    /// `zdiff3` conflict styles).
    Base,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Resolution::Ours => "ours",
            Resolution::Theirs => "theirs",
            Resolution::Both => "both sides",
            Resolution::Base => "the common ancestor",
        }
    }
}

/// One conflict: the marker lines that bound it, and the line ranges of
/// each side. Every index is a 0-based index into [`Conflicted::lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `<<<<<<<` line.
    pub start: usize,
    /// Our lines, as `[lo, hi)`.
    pub ours: (usize, usize),
    /// The common ancestor's lines, when git wrote them.
    pub base: Option<(usize, usize)>,
    /// Their lines, as `[lo, hi)`.
    pub theirs: (usize, usize),
    /// The `>>>>>>>` line.
    pub end: usize,
    /// What the `<<<<<<<` line named — usually `HEAD`.
    pub ours_label: String,
    /// What the `>>>>>>>` line named — usually the incoming branch.
    pub theirs_label: String,
}

impl Hunk {
    /// Number of lines on each side, for the menu and the status line.
    pub fn counts(&self) -> (usize, usize) {
        (self.ours.1 - self.ours.0, self.theirs.1 - self.theirs.0)
    }
}

/// One side of a conflicted file, rebuilt as plain text.
#[derive(Debug, Default, Clone)]
pub struct Sides {
    /// The file with every hunk resolved to our side.
    pub ours: String,
    /// The file with every hunk resolved to their side.
    pub theirs: String,
    /// The hunk each line of `ours` belongs to, indexed by 0-based line.
    /// `None` marks a line both sides agree on.
    pub ours_owner: Vec<Option<usize>>,
    /// The same, for `theirs`.
    pub theirs_owner: Vec<Option<usize>>,
}

/// A working-tree file that still holds conflict markers.
#[derive(Debug, Clone)]
pub struct Conflicted {
    /// Every line of the file, without its newline.
    lines: Vec<String>,
    /// True when the file ends with a newline, so a rewrite keeps it.
    ends_with_newline: bool,
    /// The conflicts, in the order they appear in the file.
    pub hunks: Vec<Hunk>,
}

#[allow(clippy::len_without_is_empty)]
impl Conflicted {
    /// Read the markers out of `text`.
    ///
    /// Returns `None` when the file holds no complete conflict. That is
    /// the normal answer for most files, and also for a conflict git could
    /// not express with markers — a file one side deleted, for example.
    /// An unterminated run is dropped rather than guessed at.
    pub fn parse(text: &str) -> Option<Self> {
        let ends_with_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        // An empty file has one empty line under this split; it has no
        // markers either way, so the hunk check below rejects it.
        let lines: Vec<String> = body.split('\n').map(|s| s.to_string()).collect();

        let mut hunks = Vec::new();
        let mut i = 0usize;
        while i < lines.len() {
            if !is_marker(&lines[i], OURS) {
                i += 1;
                continue;
            }
            let start = i;
            let ours_label = label_of(&lines[i], OURS);
            let ours_lo = i + 1;
            let mut ours_hi = None;
            let mut base: Option<(usize, usize)> = None;
            let mut base_lo = None;
            let mut theirs_lo = None;
            let mut end = None;
            let mut theirs_label = String::new();
            let mut j = i + 1;
            while j < lines.len() {
                let line = &lines[j];
                // A second `<<<<<<<` before this one closes means the run
                // is broken. Restart from there rather than swallow it.
                if is_marker(line, OURS) {
                    break;
                }
                if is_marker(line, BASE) && base_lo.is_none() && theirs_lo.is_none() {
                    ours_hi = Some(j);
                    base_lo = Some(j + 1);
                } else if is_marker(line, SEP) && theirs_lo.is_none() {
                    match base_lo {
                        Some(lo) => base = Some((lo, j)),
                        None => ours_hi = Some(j),
                    }
                    theirs_lo = Some(j + 1);
                } else if is_marker(line, THEIRS) && theirs_lo.is_some() {
                    theirs_label = label_of(line, THEIRS);
                    end = Some(j);
                    break;
                }
                j += 1;
            }
            match (ours_hi, theirs_lo, end) {
                (Some(ours_hi), Some(theirs_lo), Some(end)) => {
                    hunks.push(Hunk {
                        start,
                        ours: (ours_lo, ours_hi),
                        base,
                        theirs: (theirs_lo, end),
                        end,
                        ours_label,
                        theirs_label,
                    });
                    i = end + 1;
                }
                // Incomplete: step past the opening marker and look again.
                _ => i = start + 1,
            }
        }
        if hunks.is_empty() {
            return None;
        }
        Some(Conflicted {
            lines,
            ends_with_newline,
            hunks,
        })
    }

    /// How many conflicts the file holds. Never zero: a file with none
    /// does not parse (see [`Conflicted::parse`]).
    pub fn len(&self) -> usize {
        self.hunks.len()
    }

    /// The branch names the first hunk carries. Every hunk of one merge
    /// names the same two, so one pair labels the whole file.
    pub fn labels(&self) -> (&str, &str) {
        match self.hunks.first() {
            Some(h) => (h.ours_label.as_str(), h.theirs_label.as_str()),
            None => ("ours", "theirs"),
        }
    }

    /// The two sides as plain files, plus the hunk each line came from.
    ///
    /// This is what the review pane diffs. Agreed lines are in both files
    /// and land as context rows; a hunk's lines are in one file only (or
    /// in both with different text) and land as a changed section.
    pub fn sides(&self) -> Sides {
        let mut out = Sides::default();
        let mut ours: Vec<&str> = Vec::with_capacity(self.lines.len());
        let mut theirs: Vec<&str> = Vec::with_capacity(self.lines.len());
        let mut i = 0usize;
        let mut next = 0usize;
        while i < self.lines.len() {
            match self.hunks.get(next).filter(|h| h.start == i) {
                Some(h) => {
                    for line in &self.lines[h.ours.0..h.ours.1] {
                        ours.push(line);
                        out.ours_owner.push(Some(next));
                    }
                    for line in &self.lines[h.theirs.0..h.theirs.1] {
                        theirs.push(line);
                        out.theirs_owner.push(Some(next));
                    }
                    i = h.end + 1;
                    next += 1;
                }
                None => {
                    ours.push(&self.lines[i]);
                    theirs.push(&self.lines[i]);
                    out.ours_owner.push(None);
                    out.theirs_owner.push(None);
                    i += 1;
                }
            }
        }
        out.ours = self.join(&ours);
        out.theirs = self.join(&theirs);
        out
    }

    /// Join lines back into file text, keeping the file's own last line.
    fn join(&self, lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        if self.ends_with_newline && !s.is_empty() {
            s.push('\n');
        }
        s
    }

    /// The file with the listed hunks resolved. A hunk with no entry in
    /// `picks` keeps its markers, so resolving one conflict never touches
    /// the others.
    pub fn apply(&self, picks: &HashMap<usize, Resolution>) -> String {
        let mut out: Vec<&str> = Vec::with_capacity(self.lines.len());
        let mut i = 0usize;
        let mut next = 0usize;
        while i < self.lines.len() {
            match self.hunks.get(next).filter(|h| h.start == i) {
                Some(h) => {
                    let id = next;
                    next += 1;
                    i = h.end + 1;
                    match picks.get(&id) {
                        Some(Resolution::Ours) => {
                            out.extend(self.lines[h.ours.0..h.ours.1].iter().map(String::as_str));
                        }
                        Some(Resolution::Theirs) => {
                            out.extend(
                                self.lines[h.theirs.0..h.theirs.1]
                                    .iter()
                                    .map(String::as_str),
                            );
                        }
                        Some(Resolution::Both) => {
                            out.extend(self.lines[h.ours.0..h.ours.1].iter().map(String::as_str));
                            out.extend(
                                self.lines[h.theirs.0..h.theirs.1]
                                    .iter()
                                    .map(String::as_str),
                            );
                        }
                        Some(Resolution::Base) => {
                            // No ancestor in this hunk: keep our side
                            // rather than delete lines nobody asked about.
                            let r = h.base.unwrap_or(h.ours);
                            out.extend(self.lines[r.0..r.1].iter().map(String::as_str));
                        }
                        // Untouched: the markers stay exactly as they are.
                        None => {
                            out.extend(self.lines[h.start..=h.end].iter().map(String::as_str));
                        }
                    }
                }
                None => {
                    out.push(&self.lines[i]);
                    i += 1;
                }
            }
        }
        self.join(&out)
    }

    /// The file with one hunk resolved.
    pub fn resolve_one(&self, idx: usize, how: Resolution) -> String {
        let mut picks = HashMap::new();
        picks.insert(idx, how);
        self.apply(&picks)
    }

    /// The file with every hunk resolved the same way.
    pub fn resolve_all(&self, how: Resolution) -> String {
        let picks = (0..self.hunks.len()).map(|i| (i, how)).collect();
        self.apply(&picks)
    }
}

/// True when `text` holds a conflict marker at the start of any line.
/// Cheaper than a full parse, for the file panel's warning icon.
pub fn has_markers(text: &str) -> bool {
    text.lines()
        .any(|l| is_marker(l, OURS) || is_marker(l, THEIRS))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "\
one
<<<<<<< HEAD
ours-a
ours-b
=======
theirs-a
>>>>>>> feature
two
";

    const DIFF3: &str = "\
one
<<<<<<< HEAD
ours-a
||||||| merged common ancestors
base-a
=======
theirs-a
>>>>>>> feature
two
";

    #[test]
    fn a_file_with_no_markers_is_not_a_conflict() {
        assert!(Conflicted::parse("one\ntwo\n").is_none());
        assert!(Conflicted::parse("").is_none());
        // A markdown heading underline is not a separator on its own.
        assert!(Conflicted::parse("Title\n=======\nbody\n").is_none());
    }

    #[test]
    fn the_two_sides_and_their_labels_come_out_of_the_markers() {
        let c = Conflicted::parse(PLAIN).expect("one conflict");
        assert_eq!(c.len(), 1);
        assert_eq!(c.labels(), ("HEAD", "feature"));
        assert!(c.hunks[0].base.is_none());
        assert_eq!(c.hunks[0].counts(), (2, 1));
        let s = c.sides();
        assert_eq!(s.ours, "one\nours-a\nours-b\ntwo\n");
        assert_eq!(s.theirs, "one\ntheirs-a\ntwo\n");
        // The agreed lines belong to no hunk; the rest belong to hunk 0.
        assert_eq!(s.ours_owner, vec![None, Some(0), Some(0), None]);
        assert_eq!(s.theirs_owner, vec![None, Some(0), None]);
    }

    #[test]
    fn the_common_ancestor_is_read_when_git_writes_one() {
        let c = Conflicted::parse(DIFF3).expect("one conflict");
        assert!(c.hunks[0].base.is_some());
        let s = c.sides();
        assert_eq!(s.ours, "one\nours-a\ntwo\n");
        assert_eq!(s.theirs, "one\ntheirs-a\ntwo\n");
        assert_eq!(c.resolve_all(Resolution::Base), "one\nbase-a\ntwo\n");
    }

    /// `zdiff3` writes the ancestor marker with no label at all.
    #[test]
    fn an_unlabelled_ancestor_marker_still_parses() {
        let text = "<<<<<<< HEAD\na\n|||||||\nb\n=======\nc\n>>>>>>> other\n";
        let c = Conflicted::parse(text).expect("one conflict");
        assert!(c.hunks[0].base.is_some());
        assert_eq!(c.resolve_all(Resolution::Base), "b\n");
    }

    #[test]
    fn each_side_can_be_taken_on_its_own() {
        let c = Conflicted::parse(PLAIN).expect("one conflict");
        assert_eq!(
            c.resolve_one(0, Resolution::Ours),
            "one\nours-a\nours-b\ntwo\n"
        );
        assert_eq!(c.resolve_one(0, Resolution::Theirs), "one\ntheirs-a\ntwo\n");
        assert_eq!(
            c.resolve_one(0, Resolution::Both),
            "one\nours-a\nours-b\ntheirs-a\ntwo\n"
        );
    }

    #[test]
    fn resolving_one_conflict_leaves_the_others_alone() {
        let text = "\
<<<<<<< HEAD
a1
=======
b1
>>>>>>> f
middle
<<<<<<< HEAD
a2
=======
b2
>>>>>>> f
";
        let c = Conflicted::parse(text).expect("two conflicts");
        assert_eq!(c.len(), 2);
        let out = c.resolve_one(0, Resolution::Theirs);
        assert_eq!(
            out,
            "b1\nmiddle\n<<<<<<< HEAD\na2\n=======\nb2\n>>>>>>> f\n"
        );
        // The rewritten file parses again, with one conflict left.
        let again = Conflicted::parse(&out).expect("one conflict left");
        assert_eq!(again.len(), 1);
        assert_eq!(again.resolve_all(Resolution::Ours), "b1\nmiddle\na2\n");
    }

    #[test]
    fn an_empty_side_resolves_to_nothing() {
        let text = "keep\n<<<<<<< HEAD\n=======\nadded\n>>>>>>> f\n";
        let c = Conflicted::parse(text).expect("one conflict");
        assert_eq!(c.hunks[0].counts(), (0, 1));
        assert_eq!(c.resolve_one(0, Resolution::Ours), "keep\n");
        assert_eq!(c.resolve_one(0, Resolution::Theirs), "keep\nadded\n");
    }

    #[test]
    fn a_file_with_no_final_newline_keeps_it_that_way() {
        let text = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> f";
        let c = Conflicted::parse(text).expect("one conflict");
        assert_eq!(c.resolve_all(Resolution::Ours), "a");
        assert_eq!(c.sides().theirs, "b");
    }

    #[test]
    fn an_unterminated_run_is_not_a_conflict() {
        assert!(Conflicted::parse("<<<<<<< HEAD\na\n=======\nb\n").is_none());
        assert!(Conflicted::parse("<<<<<<< HEAD\na\n").is_none());
        // The separator alone means nothing without an opening marker.
        assert!(Conflicted::parse("a\n=======\nb\n>>>>>>> f\n").is_none());
    }

    /// A stray opening marker before a real conflict must not swallow it.
    #[test]
    fn a_broken_run_before_a_good_one_is_skipped() {
        let text = "<<<<<<< stray\n<<<<<<< HEAD\na\n=======\nb\n>>>>>>> f\n";
        let c = Conflicted::parse(text).expect("the good conflict");
        assert_eq!(c.len(), 1);
        assert_eq!(c.resolve_all(Resolution::Ours), "<<<<<<< stray\na\n");
    }

    #[test]
    fn markers_are_seven_characters_and_then_a_space_or_nothing() {
        assert!(has_markers("<<<<<<< HEAD\n"));
        assert!(has_markers(">>>>>>>\n"));
        // Eight of them, or a marker with no gap before the label.
        assert!(!has_markers("<<<<<<<<HEAD\n"));
        assert!(!has_markers("  <<<<<<< HEAD\n"));
        assert!(!has_markers("plain text\n"));
    }
}
