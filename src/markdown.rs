//! Markdown → styled terminal lines, for the preview pane.
//!
//! The pipeline has two halves that are deliberately kept apart:
//!
//!   1. [`parse`] reads the source once into a flat list of [`Node`]s.
//!      Fenced code is syntax-highlighted here, because that is the
//!      expensive part and it does not depend on how wide the pane is.
//!   2. [`Doc::lay_out`] turns those nodes into ratatui lines for one
//!      width. Dragging the divider re-runs only this half.
//!
//! Block structure is flat rather than a tree: a block quote sets a depth
//! on the blocks inside it instead of nesting them. Everything a reader
//! meets in a plan file or a review write-up renders correctly, and the
//! layout pass stays a simple loop.

use crate::highlight::{self, HlLine};
use crate::theme::palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Extensions that open in the preview instead of the editor.
const MD_EXT: &[&str] = &["md", "markdown", "mdown", "mkd", "mdx", "mdc"];

/// True when `path` names a markdown file.
pub fn is_markdown(path: &str) -> bool {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    MD_EXT.contains(&ext.as_str())
}

// ----------------------------------------------------------------- inline

/// What an inline run of text is. Flags rather than a tree: the styles
/// compose (bold code inside a link is all three at once), which is
/// exactly what a terminal cell can express.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Ink {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: bool,
    /// The URL half of a link, drawn dim after the text.
    url: bool,
}

/// One run of text that shares a single style.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Run {
    text: String,
    ink: Ink,
}

impl Run {
    fn style(&self) -> Style {
        let p = palette();
        let i = self.ink;
        let mut s = Style::default().fg(if i.code {
            p.code
        } else if i.url {
            p.faint
        } else if i.link {
            p.key
        } else {
            p.text
        });
        if i.code {
            s = s.bg(p.cursor);
        }
        if i.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if i.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if i.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if i.link && !i.url {
            s = s.add_modifier(Modifier::UNDERLINED);
        }
        s
    }
}

fn push_run(out: &mut Vec<Run>, text: String, ink: Ink) {
    if text.is_empty() {
        return;
    }
    // Merging equal-styled neighbours keeps the span count (and so the
    // wrap loop) proportional to the styling, not to the character count.
    if let Some(last) = out.last_mut() {
        if last.ink == ink {
            last.text.push_str(&text);
            return;
        }
    }
    out.push(Run { text, ink });
}

fn is_ascii_punct(b: u8) -> bool {
    b.is_ascii_punctuation()
}

/// Length of the run of byte `c` starting at `i`.
fn run_len(b: &[u8], i: usize, c: u8) -> usize {
    let mut n = 0;
    while i + n < b.len() && b[i + n] == c {
        n += 1;
    }
    n
}

/// Start of the next run of exactly `n` copies of `c` at or after `from`.
fn find_run(b: &[u8], from: usize, c: u8, n: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == c {
            let len = run_len(b, i, c);
            if len == n {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// End of a bracketed span that starts at `open` (which holds `open_c`),
/// counting nesting. Returns the index of the closing byte.
fn match_bracket(b: &[u8], open: usize, open_c: u8, close_c: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            c if c == open_c => depth += 1,
            c if c == close_c => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// True when the byte at `i` is a word character — the test that keeps
/// `snake_case_name` from turning into italics halfway through.
fn word_byte(b: &[u8], i: usize) -> bool {
    b.get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c >= 0x80)
}

/// Parse inline markdown into styled runs.
fn inline(src: &str) -> Vec<Run> {
    let mut out = Vec::new();
    inline_into(src, Ink::default(), &mut out);
    out
}

fn inline_into(src: &str, ink: Ink, out: &mut Vec<Run>) {
    let b = src.as_bytes();
    let mut plain = String::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        // A backslash escape is the one thing that beats every delimiter.
        if c == b'\\' && i + 1 < b.len() && is_ascii_punct(b[i + 1]) {
            plain.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        // Code spans win over emphasis, so `a*b*c` stays literal.
        if c == b'`' {
            let n = run_len(b, i, b'`');
            if let Some(end) = find_run(b, i + n, b'`', n) {
                push_run(out, std::mem::take(&mut plain), ink);
                let text = &src[i + n..end];
                // CommonMark drops one space on each side, so `` ` `` works.
                let text = match (text.starts_with(' '), text.ends_with(' ')) {
                    (true, true) if text.trim().len() < text.len() - 1 => &text[1..text.len() - 1],
                    _ => text,
                };
                push_run(
                    out,
                    text.replace('\n', " "),
                    Ink {
                        code: true,
                        italic: false,
                        ..ink
                    },
                );
                i = end + n;
                continue;
            }
        }
        // Links and images. An image renders as its alt text with a mark,
        // because a terminal cannot show the picture.
        let img = c == b'!' && b.get(i + 1) == Some(&b'[');
        if c == b'[' || img {
            let open = if img { i + 1 } else { i };
            if let Some(close) = match_bracket(b, open, b'[', b']') {
                let is_link = b.get(close + 1) == Some(&b'(');
                if is_link {
                    if let Some(pclose) = match_bracket(b, close + 1, b'(', b')') {
                        push_run(out, std::mem::take(&mut plain), ink);
                        let text = &src[open + 1..close];
                        let url = src[close + 2..pclose]
                            .split_whitespace()
                            .next()
                            .unwrap_or("");
                        let label = Ink { link: true, ..ink };
                        if img {
                            push_run(out, "🖼 ".to_string(), label);
                        }
                        if text.is_empty() {
                            push_run(out, url.to_string(), label);
                        } else {
                            inline_into(text, label, out);
                            // The address is kept: nothing in a terminal can
                            // follow a link, so hiding it loses the only copy.
                            if url != text && !url.is_empty() {
                                push_run(
                                    out,
                                    format!(" ({url})"),
                                    Ink {
                                        url: true,
                                        link: true,
                                        bold: false,
                                        ..ink
                                    },
                                );
                            }
                        }
                        i = pclose + 1;
                        continue;
                    }
                }
                // `[ ]` / `[x]` and reference links have no address to show,
                // so they render as their own text.
            }
        }
        // Autolink: <https://…> or <someone@example.com>.
        if c == b'<' {
            if let Some(close) = b[i..].iter().position(|&x| x == b'>').map(|n| i + n) {
                let body = &src[i + 1..close];
                let looks_like = body.contains("://") || (body.contains('@') && body.contains('.'));
                if looks_like && !body.contains(' ') {
                    push_run(out, std::mem::take(&mut plain), ink);
                    push_run(out, body.to_string(), Ink { link: true, ..ink });
                    i = close + 1;
                    continue;
                }
            }
        }
        // Strikethrough.
        if c == b'~' && run_len(b, i, b'~') == 2 {
            if let Some(end) = find_run(b, i + 2, b'~', 2) {
                push_run(out, std::mem::take(&mut plain), ink);
                inline_into(
                    &src[i + 2..end],
                    Ink {
                        strike: true,
                        ..ink
                    },
                    out,
                );
                i = end + 2;
                continue;
            }
        }
        // Emphasis. `_` only opens at a word boundary, so identifiers such
        // as MAX_RETRY_COUNT survive intact — they are everywhere in the
        // files this preview exists to read.
        if c == b'*' || c == b'_' {
            let n = run_len(b, i, c).min(3);
            let opens = b
                .get(i + n)
                .is_some_and(|x| !x.is_ascii_whitespace() && *x != c);
            let boundary = c == b'*' || !word_byte(b, i.wrapping_sub(1));
            if opens && boundary {
                if let Some(end) = find_emph_close(b, i + n, c, n) {
                    push_run(out, std::mem::take(&mut plain), ink);
                    let inner = Ink {
                        bold: ink.bold || n >= 2,
                        italic: ink.italic || n == 1 || n == 3,
                        ..ink
                    };
                    inline_into(&src[i + n..end], inner, out);
                    i = end + n;
                    continue;
                }
            }
        }
        // Anything else is literal text; step one whole character.
        let ch = src[i..].chars().next().expect("index is a char boundary");
        plain.push(ch);
        i += ch.len_utf8();
    }
    push_run(out, plain, ink);
}

/// The closing delimiter for an emphasis run: `n` copies of `c` that are
/// not preceded by a space, and (for `_`) not inside a word.
fn find_emph_close(b: &[u8], from: usize, c: u8, n: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == c {
            let len = run_len(b, i, c);
            let closes = i > from && !b[i - 1].is_ascii_whitespace();
            let boundary = c == b'*' || !word_byte(b, i + len);
            if len >= n && closes && boundary {
                return Some(i);
            }
            i += len;
            continue;
        }
        i += 1;
    }
    None
}

/// The text of a run list with no styling — for width arithmetic.
fn plain_text(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

// ------------------------------------------------------------------ blocks

/// What a list item is numbered or bulleted with.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Marker {
    Bullet,
    Number(u64),
    /// A GitHub task box, and whether it is ticked.
    Task(bool),
}

/// How one table column lines up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug)]
enum Block {
    Heading {
        level: u8,
        runs: Vec<Run>,
    },
    Para(Vec<Run>),
    Code {
        tag: String,
        /// Raw text per line, for a block loupe cannot highlight.
        raw: Vec<String>,
        /// Highlighted spans per line; empty when there is no syntax for
        /// the tag.
        hl: Vec<HlLine>,
    },
    Item {
        /// Nesting level, already normalized from the source indent.
        depth: usize,
        marker: Marker,
        runs: Vec<Run>,
    },
    Rule,
    Table {
        head: Vec<Vec<Run>>,
        align: Vec<Align>,
        rows: Vec<Vec<Vec<Run>>>,
    },
    /// The YAML header some plan files start with.
    FrontMatter(Vec<String>),
    /// Raw HTML, shown as itself rather than guessed at.
    Html(Vec<String>),
    Blank,
}

/// One block plus where it came from.
#[derive(Debug)]
struct Node {
    block: Block,
    /// Block-quote nesting, 0 for ordinary text.
    quote: usize,
    /// 1-based line in the source, for the preview ⇄ source jump.
    src: usize,
}

/// A parsed document: blocks, plus the layout of the last width asked for.
pub struct Doc {
    nodes: Vec<Node>,
    /// Rendered lines for `width`, and the source line each one came from.
    lines: Vec<Line<'static>>,
    src_of: Vec<usize>,
    /// Indices into `lines` of every heading, for `}` and `{`.
    heads: Vec<usize>,
    width: usize,
}

impl Doc {
    /// The rendered lines. Call [`Doc::lay_out`] first for the right width.
    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The source line the rendered row `row` came from, 1-based.
    pub fn source_line(&self, row: usize) -> usize {
        self.src_of.get(row).copied().unwrap_or(1)
    }

    /// The rendered row that best shows source line `line` (1-based).
    pub fn row_of_source(&self, line: usize) -> usize {
        // The first row that renders that very line, so a heading lands
        // on its text and not on the rule under it.
        if let Some(row) = self.src_of.iter().position(|s| *s == line) {
            return row;
        }
        // The line renders nothing of its own (it is inside a paragraph,
        // or it is blank): stay on the block that contains it.
        let mut best = 0;
        for (row, src) in self.src_of.iter().enumerate() {
            if *src < line {
                best = row;
            } else {
                break;
            }
        }
        best
    }

    /// The next heading row after `row`, or the previous one before it.
    pub fn heading_near(&self, row: usize, forward: bool) -> Option<usize> {
        if forward {
            self.heads.iter().copied().find(|h| *h > row)
        } else {
            self.heads.iter().copied().rev().find(|h| *h < row)
        }
    }

    /// Re-lay the document for a pane `width` columns wide. Cheap and a
    /// no-op when the width has not moved.
    pub fn lay_out(&mut self, width: usize) {
        let width = width.max(8);
        if width == self.width && !self.lines.is_empty() {
            return;
        }
        self.width = width;
        let (lines, src_of, heads) = lay_out(&self.nodes, width);
        self.lines = lines;
        self.src_of = src_of;
        self.heads = heads;
    }

    /// Force the next [`Doc::lay_out`] to redraw — after a theme change,
    /// where the widths are unchanged but every color moved.
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.width = 0;
    }
}

/// Parse a markdown document. Syntax highlighting of fenced blocks happens
/// here, so it is paid once per file rather than once per resize.
pub fn parse(src: &str) -> Doc {
    let raw: Vec<&str> = src.lines().collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut i = 0usize;

    // A `---` fence on the very first line is front matter, not a rule.
    if raw.first().is_some_and(|l| l.trim_end() == "---") {
        let mut j = 1;
        while j < raw.len() && raw[j].trim_end() != "---" {
            j += 1;
        }
        if j < raw.len() {
            nodes.push(Node {
                block: Block::FrontMatter(raw[1..j].iter().map(|s| s.to_string()).collect()),
                quote: 0,
                src: 1,
            });
            i = j + 1;
        }
    }

    // Paragraph text waiting to be flushed, with the line it started on.
    let mut para: Vec<String> = Vec::new();
    let mut para_at = 0usize;
    let mut para_quote = 0usize;

    while i < raw.len() {
        let (quote, line) = strip_quote(raw[i]);
        let trimmed = line.trim();
        let src = i + 1;

        // A quote level change ends the paragraph it was part of.
        if !para.is_empty() && quote != para_quote {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
        }

        // --- fenced code
        if let Some((fence, tag)) = code_fence(line) {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
            let mut body: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < raw.len() {
                let (_, inner) = strip_quote(raw[j]);
                if closes_fence(inner, fence) {
                    break;
                }
                body.push(inner.to_string());
                j += 1;
            }
            let joined = if body.is_empty() {
                String::new()
            } else {
                format!("{}\n", body.join("\n"))
            };
            let hl = highlight::highlight_block(&tag, &joined);
            nodes.push(Node {
                block: Block::Code { tag, raw: body, hl },
                quote,
                src,
            });
            i = j + 1;
            continue;
        }

        // --- blank
        if trimmed.is_empty() {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
            nodes.push(Node {
                block: Block::Blank,
                quote,
                src,
            });
            i += 1;
            continue;
        }

        // --- ATX heading
        if let Some((level, text)) = atx_heading(trimmed) {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
            nodes.push(Node {
                block: Block::Heading {
                    level,
                    runs: inline(text),
                },
                quote,
                src,
            });
            i += 1;
            continue;
        }

        // --- setext heading: the underline turns the paragraph above it
        // into a heading, so it is checked before the thematic break.
        if !para.is_empty() {
            if let Some(level) = setext(trimmed) {
                let text = para.join(" ");
                para.clear();
                nodes.push(Node {
                    block: Block::Heading {
                        level,
                        runs: inline(&text),
                    },
                    quote: para_quote,
                    src: para_at,
                });
                i += 1;
                continue;
            }
        }

        // --- thematic break
        if is_rule(trimmed) {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
            nodes.push(Node {
                block: Block::Rule,
                quote,
                src,
            });
            i += 1;
            continue;
        }

        // --- table: a header row followed by a delimiter row
        if line.contains('|') && i + 1 < raw.len() {
            let (_, next) = strip_quote(raw[i + 1]);
            if let Some(align) = table_delims(next) {
                let head = split_row(line);
                if head.len() == align.len() {
                    flush_para(&mut nodes, &mut para, para_at, para_quote);
                    let mut rows = Vec::new();
                    let mut j = i + 2;
                    while j < raw.len() {
                        let (_, r) = strip_quote(raw[j]);
                        if !r.contains('|') || r.trim().is_empty() {
                            break;
                        }
                        let mut cells = split_row(r);
                        cells.resize(align.len(), String::new());
                        rows.push(cells.iter().map(|c| inline(c)).collect());
                        j += 1;
                    }
                    nodes.push(Node {
                        block: Block::Table {
                            head: head.iter().map(|c| inline(c)).collect(),
                            align,
                            rows,
                        },
                        quote,
                        src,
                    });
                    i = j;
                    continue;
                }
            }
        }

        // --- list item
        if let Some((indent, marker, rest)) = list_item(line) {
            flush_para(&mut nodes, &mut para, para_at, para_quote);
            // Continuation lines: indented further than the marker and not
            // a new item of their own.
            let mut text = rest.to_string();
            let mut j = i + 1;
            while j < raw.len() {
                let (q2, cont) = strip_quote(raw[j]);
                if q2 != quote || cont.trim().is_empty() || list_item(cont).is_some() {
                    break;
                }
                if leading_spaces(cont) <= indent {
                    break;
                }
                text.push(' ');
                text.push_str(cont.trim());
                j += 1;
            }
            nodes.push(Node {
                block: Block::Item {
                    depth: indent / 2,
                    marker,
                    runs: inline(&text),
                },
                quote,
                src,
            });
            i = j;
            continue;
        }

        // --- raw HTML block
        if para.is_empty() && trimmed.starts_with('<') && !trimmed.starts_with("<http") {
            let mut body = Vec::new();
            let mut j = i;
            while j < raw.len() {
                let (_, l) = strip_quote(raw[j]);
                if l.trim().is_empty() {
                    break;
                }
                body.push(l.to_string());
                j += 1;
            }
            nodes.push(Node {
                block: Block::Html(body),
                quote,
                src,
            });
            i = j;
            continue;
        }

        // --- paragraph text
        if para.is_empty() {
            para_at = src;
            para_quote = quote;
        }
        para.push(trimmed.to_string());
        i += 1;
    }
    flush_para(&mut nodes, &mut para, para_at, para_quote);

    Doc {
        nodes,
        lines: Vec::new(),
        src_of: Vec::new(),
        heads: Vec::new(),
        width: 0,
    }
}

fn flush_para(nodes: &mut Vec<Node>, para: &mut Vec<String>, at: usize, quote: usize) {
    if para.is_empty() {
        return;
    }
    let text = para.join(" ");
    para.clear();
    nodes.push(Node {
        block: Block::Para(inline(&text)),
        quote,
        src: at,
    });
}

/// Peel off `>` markers, returning the depth and what is left.
fn strip_quote(line: &str) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = line;
    loop {
        let t = rest.trim_start_matches(' ');
        // More than three spaces of indent is code, not a quote marker.
        if rest.len() - t.len() > 3 && depth == 0 {
            return (depth, rest);
        }
        match t.strip_prefix('>') {
            Some(r) => {
                depth += 1;
                rest = r.strip_prefix(' ').unwrap_or(r);
            }
            None => return (depth, rest),
        }
    }
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// `(fence character and length, info string)` when the line opens a fence.
fn code_fence(line: &str) -> Option<((char, usize), String)> {
    let t = line.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    if n < 3 {
        return None;
    }
    let info = t[n..].trim().to_string();
    // A ``` info string may not itself contain a backtick.
    if c == '`' && info.contains('`') {
        return None;
    }
    Some(((c, n), info))
}

fn closes_fence(line: &str, fence: (char, usize)) -> bool {
    let t = line.trim();
    let n = t.chars().take_while(|x| *x == fence.0).count();
    n >= fence.1 && t.len() == n
}

fn atx_heading(t: &str) -> Option<(u8, &str)> {
    let n = t.chars().take_while(|c| *c == '#').count();
    if n == 0 || n > 6 {
        return None;
    }
    let rest = &t[n..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((n as u8, text))
}

fn setext(t: &str) -> Option<u8> {
    if t.len() < 2 {
        return None;
    }
    if t.chars().all(|c| c == '=') {
        return Some(1);
    }
    if t.chars().all(|c| c == '-') {
        return Some(2);
    }
    None
}

fn is_rule(t: &str) -> bool {
    let squeezed: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    squeezed.len() >= 3
        && (squeezed.chars().all(|c| c == '-')
            || squeezed.chars().all(|c| c == '*')
            || squeezed.chars().all(|c| c == '_'))
}

/// `(indent, marker, text)` when the line starts a list item.
fn list_item(line: &str) -> Option<(usize, Marker, &str)> {
    let indent = leading_spaces(line);
    let t = &line[indent..];
    let mut chars = t.chars();
    let first = chars.next()?;
    let rest = if first == '-' || first == '*' || first == '+' {
        let r = &t[1..];
        if !r.starts_with(' ') {
            return None;
        }
        r.trim_start()
    } else if first.is_ascii_digit() {
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        // A four-digit "list" is almost always a year, not a list.
        if digits > 3 {
            return None;
        }
        let after = &t[digits..];
        let d = after.chars().next()?;
        if d != '.' && d != ')' {
            return None;
        }
        let r = &after[1..];
        if !r.starts_with(' ') {
            return None;
        }
        let n = t[..digits].parse().unwrap_or(1);
        let text = r.trim_start();
        return Some((indent, Marker::Number(n), text));
    } else {
        return None;
    };
    // A task box turns the bullet into a checkbox.
    let lower = rest.to_ascii_lowercase();
    if let Some(after) = lower.strip_prefix("[ ] ").map(|_| &rest[4..]) {
        return Some((indent, Marker::Task(false), after));
    }
    if lower.starts_with("[x] ") {
        return Some((indent, Marker::Task(true), &rest[4..]));
    }
    Some((indent, Marker::Bullet, rest))
}

/// The alignment row under a table header, or None when the line is not one.
fn table_delims(line: &str) -> Option<Vec<Align>> {
    let cells = split_row(line);
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for c in &cells {
        let t = c.trim();
        let body = t.trim_start_matches(':').trim_end_matches(':');
        if body.is_empty() || !body.chars().all(|x| x == '-') {
            return None;
        }
        out.push(match (t.starts_with(':'), t.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

/// Split one table row on unescaped pipes, dropping the outer pair.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut esc = false;
    for c in t.chars() {
        if esc {
            // The pipe is the one character a cell has to escape.
            if c != '|' {
                cur.push('\\');
            }
            cur.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

// ------------------------------------------------------------------ layout

fn w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut a row of spans to `width` columns, keeping the styles.
fn clip(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| w(&s.content)).sum();
    if total <= width {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0;
    for span in spans {
        let sw = w(&span.content);
        if used + sw <= width {
            used += sw;
            out.push(span);
            continue;
        }
        let (head, _) = split_at_width(&span.content, width - used);
        if !head.is_empty() {
            out.push(Span::styled(head.to_string(), span.style));
        }
        break;
    }
    out
}

/// True when the last laid-out row holds nothing but space. Two blank
/// rows in a row read as a gap the author did not write.
fn ends_blank(lines: &[Line<'static>]) -> bool {
    match lines.last() {
        Some(l) => l.spans.iter().all(|s| s.content.trim().is_empty()),
        None => true,
    }
}

/// Wrap styled runs to `width`, with `first` in front of the first line and
/// `cont` in front of the rest. Both prefixes are already-styled spans.
fn wrap(
    runs: &[Run],
    width: usize,
    first: &[Span<'static>],
    cont: &[Span<'static>],
) -> Vec<Line<'static>> {
    let first_w: usize = first.iter().map(|s| w(&s.content)).sum();
    let cont_w: usize = cont.iter().map(|s| w(&s.content)).sum();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = first.to_vec();
    let mut used = first_w;
    // Whether anything has been added since the last line break. Counting
    // spans instead would drop the last line of a list item, whose
    // continuation prefix is one span where the first-line prefix is two.
    let mut filled = false;

    // Words carry their style with them, so a wrap never loses one. The
    // pending separator lives outside the run loop — the space between a
    // word and the `code span` after it belongs to neither run, and
    // resetting it per run is what eats it — and it keeps the style of
    // the run it came *from*, so a code span's background box starts at
    // its first character rather than one cell early.
    let mut pending: Option<Style> = None;
    for run in runs {
        let style = run.style();
        for (n, word) in run.text.split(' ').enumerate() {
            if n > 0 {
                pending = Some(style);
            }
            if word.is_empty() {
                continue;
            }
            let sep_style = pending.take();
            let sep = sep_style.is_some() && used > (if out.is_empty() { first_w } else { cont_w });
            let need = w(word) + usize::from(sep);
            if used + need > width && filled {
                out.push(Line::from(std::mem::take(&mut cur)));
                cur = cont.to_vec();
                used = cont_w;
                filled = false;
            } else if let (true, Some(sep_style)) = (sep, sep_style) {
                cur.push(Span::styled(" ".to_string(), sep_style));
                used += 1;
                filled = true;
            }
            // A single word wider than the pane (a URL, a long identifier)
            // is cut across lines rather than allowed to overflow.
            let mut rest = word;
            while used + w(rest) > width {
                let room = width.saturating_sub(used);
                if room == 0 {
                    // No room left on this line at all: break first, then
                    // carry on cutting on the next one.
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur = cont.to_vec();
                    used = cont_w;
                    filled = false;
                    continue;
                }
                let (head, tail) = split_at_width(rest, room);
                cur.push(Span::styled(head.to_string(), style));
                out.push(Line::from(std::mem::take(&mut cur)));
                cur = cont.to_vec();
                used = cont_w;
                filled = false;
                rest = tail;
                if rest.is_empty() {
                    break;
                }
            }
            if !rest.is_empty() {
                used += w(rest);
                cur.push(Span::styled(rest.to_string(), style));
                filled = true;
            }
        }
    }
    if filled || out.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

/// Split `s` at the last char boundary that fits in `width` columns.
fn split_at_width(s: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let cw = UnicodeWidthStr::width(&s[i..i + c.len_utf8()]);
        if used + cw > width {
            return s.split_at(i.max(1));
        }
        used += cw;
    }
    (s, "")
}

/// Pad or cut a plain string to exactly `width` columns.
fn fit(s: &str, width: usize) -> String {
    let sw = w(s);
    if sw == width {
        return s.to_string();
    }
    if sw < width {
        return format!("{s}{}", " ".repeat(width - sw));
    }
    let (head, _) = split_at_width(s, width.saturating_sub(1));
    format!("{head}…")
}

/// The quote bars that go in front of every line of a quoted block.
fn quote_prefix(depth: usize) -> Vec<Span<'static>> {
    if depth == 0 {
        return Vec::new();
    }
    let p = palette();
    (0..depth)
        .map(|_| Span::styled("▏ ".to_string(), Style::default().fg(p.fold)))
        .collect()
}

fn lay_out(nodes: &[Node], width: usize) -> (Vec<Line<'static>>, Vec<usize>, Vec<usize>) {
    let p = palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut src_of: Vec<usize> = Vec::new();
    let mut heads: Vec<usize> = Vec::new();
    let pad = Span::raw(" ".to_string());

    for node in nodes {
        let q = quote_prefix(node.quote);
        let qw: usize = q.iter().map(|s| w(&s.content)).sum();
        // A column of breathing room on each side: on the left it matches
        // the diff gutter, and on the right it keeps a full-width rule off
        // the border and the scrollbar.
        let body_w = width.saturating_sub(qw + 2).max(8);
        // A macro rather than a closure: every arm below needs to read
        // `lines` (for the blank-run check and the heading rule) while
        // also appending to it, which a closure that captures it forbids.
        macro_rules! emit {
            ($l:expr) => {{
                for line in $l {
                    let mut spans = vec![pad.clone()];
                    spans.extend(q.iter().cloned());
                    spans.extend(line.spans);
                    // A last clamp: a table narrower than its own borders
                    // (or any future block that miscounts) is cut here
                    // rather than allowed to spill past the pane.
                    lines.push(Line::from(clip(spans, width)));
                    src_of.push(node.src);
                }
            }};
        }

        match &node.block {
            Block::Blank => {
                // Runs of blank source lines collapse to one blank row: a
                // document with generous spacing should not scroll twice as
                // far as it reads.
                if !lines.is_empty() && !ends_blank(&lines) {
                    emit!([Line::default()]);
                }
            }
            Block::Heading { level, runs } => {
                // A heading opens with air above it, unless the blank line
                // in the source already put it there.
                if !lines.is_empty() && !ends_blank(&lines) {
                    emit!([Line::default()]);
                }
                heads.push(lines.len());
                let (color, bold) = match level {
                    1 => (p.accent, true),
                    2 => (p.accent, true),
                    3 => (p.key, true),
                    _ => (p.dim, true),
                };
                // The level is shown by weight, color and a rule rather
                // than by counting hashes, which no one reads anyway.
                let mark = match level {
                    1 => "".to_string(),
                    2 => "".to_string(),
                    3 => "▸ ".to_string(),
                    l => format!("{} ", "·".repeat((*l as usize) - 3)),
                };
                let mut styled: Vec<Run> = Vec::new();
                for r in runs {
                    let mut r = r.clone();
                    r.ink.bold = bold;
                    styled.push(r);
                }
                let head_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
                let first = if mark.is_empty() {
                    Vec::new()
                } else {
                    vec![Span::styled(mark.clone(), Style::default().fg(color))]
                };
                // Headings take the heading color, not the body color, so
                // the run styles are overridden wholesale here.
                let text = plain_text(&styled);
                let mut out = wrap(
                    &[Run {
                        text,
                        ink: Ink {
                            bold,
                            ..Default::default()
                        },
                    }],
                    body_w,
                    &first,
                    &first,
                );
                for line in &mut out {
                    for s in &mut line.spans {
                        s.style = head_style;
                    }
                }
                // The rule runs under the heading text, not across the
                // whole pane: a short heading with a full-width bar under
                // it reads as a separator, not as a title.
                let rule_w = out
                    .iter()
                    .map(|l| l.spans.iter().map(|s| w(&s.content)).sum::<usize>())
                    .max()
                    .unwrap_or(body_w)
                    .clamp(1, body_w);
                emit!(out);
                if *level <= 2 {
                    let ch = if *level == 1 { "━" } else { "─" };
                    emit!([Line::from(Span::styled(
                        ch.repeat(rule_w),
                        Style::default().fg(if *level == 1 { p.accent } else { p.divider }),
                    ))]);
                }
            }
            Block::Para(runs) => emit!(wrap(runs, body_w, &[], &[])),
            Block::Item {
                depth,
                marker,
                runs,
            } => {
                let ind = depth.min(&6) * 2;
                let bullet = match marker {
                    Marker::Bullet => match depth % 3 {
                        0 => "•".to_string(),
                        1 => "◦".to_string(),
                        _ => "▪".to_string(),
                    },
                    Marker::Number(n) => format!("{n}."),
                    Marker::Task(true) => "☑".to_string(),
                    Marker::Task(false) => "☐".to_string(),
                };
                let color = match marker {
                    Marker::Task(true) => p.ok,
                    Marker::Task(false) => p.faint,
                    _ => p.accent,
                };
                let first = vec![
                    Span::raw(" ".repeat(ind)),
                    Span::styled(format!("{bullet} "), Style::default().fg(color)),
                ];
                let cont = vec![Span::raw(" ".repeat(ind + w(&bullet) + 1))];
                // A finished task is dimmed the way a crossed-off line is.
                let runs: Vec<Run> = if matches!(marker, Marker::Task(true)) {
                    runs.iter()
                        .map(|r| Run {
                            text: r.text.clone(),
                            ink: Ink {
                                strike: false,
                                ..r.ink
                            },
                        })
                        .collect()
                } else {
                    runs.clone()
                };
                let mut out = wrap(&runs, body_w, &first, &cont);
                if matches!(marker, Marker::Task(true)) {
                    for line in &mut out {
                        for s in &mut line.spans {
                            s.style = s.style.fg(p.viewed);
                        }
                    }
                }
                emit!(out);
            }
            Block::Rule => emit!([Line::from(Span::styled(
                "─".repeat(body_w),
                Style::default().fg(p.divider),
            ))]),
            Block::Code { tag, raw, hl } => {
                let bar = Span::styled("▌".to_string(), Style::default().fg(p.divider));
                let inner_w = body_w.saturating_sub(2).max(4);
                let mut out: Vec<Line<'static>> = Vec::new();
                if !tag.is_empty() {
                    out.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(
                            fit(&format!(" {tag}"), inner_w + 1),
                            Style::default().fg(p.faint).bg(p.cursor),
                        ),
                    ]));
                }
                for (idx, text) in raw.iter().enumerate() {
                    let segs = hl.get(idx).filter(|s| !s.is_empty());
                    // Code is wrapped, not cut: a truncated command is
                    // worse than useless, and the pane has no sideways
                    // scroll to recover it with.
                    let pieces: Vec<(ratatui::style::Color, String)> = match segs {
                        Some(s) => s.clone(),
                        None => vec![(p.code, text.clone())],
                    };
                    let mut col = 0usize;
                    let mut cur: Vec<Span<'static>> = vec![
                        bar.clone(),
                        Span::styled(" ".to_string(), Style::default().bg(p.cursor)),
                    ];
                    for (color, seg) in pieces {
                        let mut rest = seg.as_str();
                        while col + w(rest) > inner_w {
                            let (head, tail) = split_at_width(rest, inner_w - col);
                            cur.push(Span::styled(
                                head.to_string(),
                                Style::default().fg(color).bg(p.cursor),
                            ));
                            out.push(Line::from(std::mem::take(&mut cur)));
                            cur = vec![
                                bar.clone(),
                                Span::styled(
                                    "↪".to_string(),
                                    Style::default().fg(p.faint).bg(p.cursor),
                                ),
                            ];
                            col = 0;
                            rest = tail;
                            if rest.is_empty() {
                                break;
                            }
                        }
                        if !rest.is_empty() {
                            col += w(rest);
                            cur.push(Span::styled(
                                rest.to_string(),
                                Style::default().fg(color).bg(p.cursor),
                            ));
                        }
                    }
                    // Pad the row so the block reads as a solid panel.
                    if col < inner_w {
                        cur.push(Span::styled(
                            " ".repeat(inner_w - col),
                            Style::default().bg(p.cursor),
                        ));
                    }
                    out.push(Line::from(cur));
                }
                emit!(out);
            }
            Block::FrontMatter(body) => {
                let mut out = Vec::new();
                for l in body {
                    out.push(Line::from(Span::styled(
                        fit(l, body_w),
                        Style::default().fg(p.faint).bg(p.empty),
                    )));
                }
                out.push(Line::from(Span::styled(
                    "─".repeat(body_w),
                    Style::default().fg(p.divider),
                )));
                emit!(out);
            }
            Block::Html(body) => {
                let out: Vec<Line<'static>> = body
                    .iter()
                    .map(|l| Line::from(Span::styled(fit(l, body_w), Style::default().fg(p.faint))))
                    .collect();
                emit!(out);
            }
            Block::Table { head, align, rows } => {
                emit!(table(head, align, rows, body_w));
            }
        }
    }
    // A trailing blank row lets the last line scroll clear of the border.
    lines.push(Line::default());
    src_of.push(nodes.last().map(|n| n.src).unwrap_or(1));
    (lines, src_of, heads)
}

/// Lay a table out as a box-drawn grid, sharing the pane width between the
/// columns in proportion to what they hold.
fn table(
    head: &[Vec<Run>],
    align: &[Align],
    rows: &[Vec<Vec<Run>>],
    width: usize,
) -> Vec<Line<'static>> {
    let p = palette();
    let n = align.len();
    if n == 0 {
        return Vec::new();
    }
    // Widest cell per column, then shrunk to fit the pane.
    let mut cols: Vec<usize> = (0..n)
        .map(|c| {
            let mut best = head.get(c).map(|r| w(&plain_text(r))).unwrap_or(0);
            for row in rows {
                best = best.max(row.get(c).map(|r| w(&plain_text(r))).unwrap_or(0));
            }
            best.max(1)
        })
        .collect();
    // Each column costs its content plus "│ " and " ".
    let chrome = n * 3 + 1;
    let mut total: usize = cols.iter().sum::<usize>() + chrome;
    while total > width {
        let Some(widest) = cols
            .iter()
            .enumerate()
            .max_by_key(|(_, x)| **x)
            .map(|(i, _)| i)
        else {
            break;
        };
        if cols[widest] <= 3 {
            break;
        }
        cols[widest] -= 1;
        total -= 1;
    }

    let border = |left: &str, mid: &str, right: &str| -> Line<'static> {
        let mut s = String::from(left);
        for (i, c) in cols.iter().enumerate() {
            s.push_str(&"─".repeat(c + 2));
            s.push_str(if i + 1 == cols.len() { right } else { mid });
        }
        Line::from(Span::styled(s, Style::default().fg(p.divider)))
    };
    let cell = |runs: &[Run], i: usize, bold: bool| -> Vec<Span<'static>> {
        let text = plain_text(runs);
        let cw = cols[i];
        let tw = w(&text).min(cw);
        let text = fit(&text, cw);
        // Alignment is applied to the trimmed text, so a cut cell still
        // lines up with the column it belongs to.
        let text = match align[i] {
            Align::Left => text,
            Align::Right => {
                let t = text.trim_end();
                format!("{}{}", " ".repeat(cw - tw.min(w(t))), t)
            }
            Align::Center => {
                let t = text.trim_end();
                let slack = cw - w(t).min(cw);
                format!(
                    "{}{}{}",
                    " ".repeat(slack / 2),
                    t,
                    " ".repeat(slack - slack / 2)
                )
            }
        };
        let mut style = Style::default().fg(p.text);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        vec![
            Span::styled("│ ".to_string(), Style::default().fg(p.divider)),
            Span::styled(fit(&text, cw), style),
            Span::raw(" ".to_string()),
        ]
    };

    let mut out = vec![border("┌", "┬", "┐")];
    let mut hrow: Vec<Span<'static>> = Vec::new();
    for i in 0..n {
        hrow.extend(cell(head.get(i).map(|v| &v[..]).unwrap_or(&[]), i, true));
    }
    hrow.push(Span::styled(
        "│".to_string(),
        Style::default().fg(p.divider),
    ));
    out.push(Line::from(hrow));
    out.push(border("├", "┼", "┤"));
    for row in rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for i in 0..n {
            spans.extend(cell(row.get(i).map(|v| &v[..]).unwrap_or(&[]), i, false));
        }
        spans.push(Span::styled(
            "│".to_string(),
            Style::default().fg(p.divider),
        ));
        out.push(Line::from(spans));
    }
    out.push(border("└", "┴", "┘"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    /// Every rendered row as plain text, at one pane width.
    fn render(src: &str, width: usize) -> Vec<String> {
        let _guard = theme::test_lock();
        theme::set_appearance(theme::Appearance::Dark);
        let mut doc = parse(src);
        doc.lay_out(width);
        doc.lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn has(rows: &[String], want: &str) -> bool {
        rows.iter().any(|r| r.contains(want))
    }

    #[test]
    fn headings_get_a_rule_and_lists_get_bullets() {
        let rows = render("# Title\n\nText.\n\n- one\n- two\n", 40);
        assert!(has(&rows, "Title"));
        assert!(has(&rows, "━━━━━"), "an h1 is underlined: {rows:?}");
        assert!(has(&rows, "• one"));
        assert!(has(&rows, "• two"));
    }

    #[test]
    fn task_boxes_show_their_state() {
        let rows = render("- [x] done\n- [ ] not done\n", 40);
        assert!(has(&rows, "☑ done"), "{rows:?}");
        assert!(has(&rows, "☐ not done"), "{rows:?}");
    }

    #[test]
    fn inline_code_keeps_the_spaces_around_it() {
        // The space between a word and the code span after it belongs to
        // neither run, and an earlier version of the wrapper ate it.
        let rows = render("It is `git reset` and then some.\n", 60);
        assert!(has(&rows, "It is git reset and then some."), "{rows:?}");
    }

    #[test]
    fn identifiers_are_not_read_as_emphasis() {
        // Underscored names are everywhere in the files this pane exists
        // to read; italicising half of one would be worse than useless.
        let rows = render("Use MAX_RETRY_COUNT and snake_case_name here.\n", 60);
        assert!(
            has(&rows, "MAX_RETRY_COUNT and snake_case_name"),
            "{rows:?}"
        );
    }

    #[test]
    fn emphasis_and_links_render_their_text() {
        let rows = render(
            "A **bold** and *slanted* [link](https://example.com) here.\n",
            70,
        );
        let joined = rows.join(" ");
        assert!(joined.contains("bold"));
        assert!(joined.contains("slanted"));
        assert!(joined.contains("link"));
        // The address survives: nothing in a terminal can follow a link,
        // so dropping it would lose the only copy.
        assert!(joined.contains("https://example.com"), "{rows:?}");
    }

    #[test]
    fn a_fenced_block_is_drawn_with_a_bar_and_its_language() {
        let rows = render("```rust\nfn main() {}\n```\n", 50);
        assert!(has(&rows, "▌ rust"), "{rows:?}");
        assert!(has(&rows, "fn main() {}"), "{rows:?}");
    }

    #[test]
    fn a_table_is_drawn_as_a_grid() {
        let src = "| A | B |\n| --- | ---: |\n| one | two |\n";
        let rows = render(src, 60);
        assert!(has(&rows, "┌"), "{rows:?}");
        assert!(has(&rows, "│ A"), "{rows:?}");
        assert!(has(&rows, "one"), "{rows:?}");
        assert!(has(&rows, "two"), "{rows:?}");
        assert!(has(&rows, "└"), "{rows:?}");
    }

    #[test]
    fn front_matter_is_shown_not_treated_as_a_rule() {
        let rows = render("---\ntitle: Plan\n---\n\n# Body\n", 40);
        assert!(has(&rows, "title: Plan"), "{rows:?}");
        assert!(has(&rows, "Body"), "{rows:?}");
    }

    #[test]
    fn block_quotes_get_a_bar_per_level() {
        let rows = render("> outer\n>\n> > inner\n", 40);
        assert!(has(&rows, "▏ outer"), "{rows:?}");
        assert!(has(&rows, "▏ ▏ inner"), "{rows:?}");
    }

    #[test]
    fn wrapping_never_drops_a_word() {
        // Every word of the source has to survive the wrap, at any width
        // — including the last one on a line, which a span-counting flush
        // condition used to lose.
        let src = "- Staging state comes from `git status` itself, so partial \
                   staging is always shown faithfully.\n\nA plain paragraph that \
                   is long enough to need several lines at any sensible width.\n";
        for width in [12usize, 20, 33, 48, 79, 120] {
            // Below the width of a word the wrapper cuts it across rows,
            // so the check is that no character is lost, not that every
            // word stays whole.
            let flat: String = render(src, width)
                .join("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            for word in ["faithfully.", "itself,", "sensible", "width."] {
                assert!(
                    flat.contains(word),
                    "lost “{word}” at width {width}: {flat}"
                );
            }
        }
    }

    #[test]
    fn every_row_fits_the_pane() {
        let src = "# A heading that is quite long indeed\n\n\
                   Text with a veryveryverylongunbrokenidentifier_that_cannot_be_split_nicely here.\n\n\
                   - a bullet with several words in it\n  - and a nested one\n\n\
                   | col | col |\n| --- | --- |\n| x | y |\n\n\
                   ```rust\nlet x = 1; // trailing comment that runs on and on and on\n```\n";
        for width in [10usize, 16, 24, 40, 80] {
            let mut doc = parse(src);
            doc.lay_out(width);
            for line in doc.lines() {
                let used: usize = line.spans.iter().map(|s| w(&s.content)).sum();
                assert!(used <= width, "row of {used} columns in a pane of {width}");
            }
        }
    }

    #[test]
    fn the_source_map_points_back_at_the_right_line() {
        let src = "# One\n\ntext\n\n# Two\n\nmore\n";
        let _guard = theme::test_lock();
        let mut doc = parse(src);
        doc.lay_out(40);
        // The row that renders "Two" reports the source line it is on.
        let row = doc
            .lines()
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("Two")))
            .expect("the second heading is rendered");
        assert_eq!(doc.source_line(row), 5);
        // And back the other way.
        assert_eq!(doc.row_of_source(5), row);
    }

    #[test]
    fn headings_are_walkable() {
        let src = "# One\n\ntext\n\n## Two\n\nmore\n\n### Three\n";
        let _guard = theme::test_lock();
        let mut doc = parse(src);
        doc.lay_out(40);
        let first = doc.heading_near(0, true).expect("a heading after row 0");
        let second = doc.heading_near(first, true).expect("and one after that");
        assert!(second > first);
        assert_eq!(doc.heading_near(second, false), Some(first));
    }

    #[test]
    fn only_markdown_extensions_count() {
        assert!(is_markdown("PLAN.md"));
        assert!(is_markdown("docs/Review.MARKDOWN"));
        assert!(!is_markdown("src/main.rs"));
        assert!(!is_markdown("README"));
    }
}
