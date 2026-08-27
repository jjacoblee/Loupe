//! Turning a language server's message into something a person can read.
//!
//! A TypeScript error arrives as one long sentence with a reasoning
//! chain folded into it:
//!
//! ```text
//! Type '{ a: number; b: string; }' is not assignable to type 'Big'.
//!   Types of property 'a' are incompatible.
//!     Type 'number' is not assignable to type 'string'.
//! ```
//!
//! The information is all there and none of it is legible: the levels
//! are lost when the message is flattened into a status bar, the types
//! are buried in quotes, and a wide object type runs off the screen.
//!
//! This splits a message back into its parts so the panel can lay them
//! out — the claim first, then each reason indented under the one it
//! explains, with names and types picked out and a long object type
//! broken over lines at its semicolons.
//!
//! Structure and color, nothing else: a terminal has no hover card to
//! put this in, and the message is the same message either way.

/// One piece of a message. The panel styles these; the split itself is
/// plain data, which is what makes it testable without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Ordinary prose.
    Text(String),
    /// A name or a type the server quoted. Drawn in the accent color and
    /// *without* its quotes — the color says what the quotes were for.
    Quoted(String),
}

/// One line of an explanation: how deep in the reasoning chain it sits,
/// and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 0 for the claim, 1 for its reason, 2 for that reason's reason.
    pub depth: usize,
    pub parts: Vec<Part>,
}

/// Where a wide object type stops being worth keeping on one line.
const WIDE_TYPE: usize = 44;

/// Split a message into rows, deepest reason last.
///
/// Servers mark the chain with leading spaces — two per level for
/// TypeScript. A message with no indentation at all is one row, which is
/// every rustc and eslint message and most TypeScript ones.
pub fn rows(message: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for raw in message.lines() {
        let trimmed = raw.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let spaces = trimmed.len() - trimmed.trim_start().len();
        let depth = spaces / 2;
        // A sentence that only *suggests* something — "Did you mean
        // 'total'?" — is the answer, not part of the complaint. It gets
        // its own row so the panel can put an arrow beside it.
        for (i, sentence) in split_suggestion(trimmed.trim()).into_iter().enumerate() {
            out.push(Row {
                depth: depth + i,
                parts: split_quoted(&sentence),
            });
        }
    }
    if out.is_empty() {
        out.push(Row {
            depth: 0,
            parts: vec![Part::Text(message.trim().to_string())],
        });
    }
    out
}

/// Peel a trailing "Did you mean …?" off a sentence.
fn split_suggestion(text: &str) -> Vec<String> {
    match text.find("Did you mean") {
        Some(i) if i > 0 => vec![text[..i].trim().to_string(), text[i..].trim().to_string()],
        _ => vec![text.to_string()],
    }
}

/// Split prose from the `'quoted'` names and types inside it.
///
/// A quoted run that is a wide object type is broken over lines at its
/// semicolons, so `{ a: number; b: string; c: boolean }` reads as three
/// properties rather than as one line that runs off the pane.
fn split_quoted(text: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut prose = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\'' {
            prose.push(ch);
            continue;
        }
        let mut quoted = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '\'' {
                closed = true;
                break;
            }
            quoted.push(c);
        }
        if !closed {
            // An apostrophe, not a quote. Put it back and move on.
            prose.push('\'');
            prose.push_str(&quoted);
            continue;
        }
        if !prose.is_empty() {
            parts.push(Part::Text(std::mem::take(&mut prose)));
        }
        parts.push(Part::Quoted(wrap_type(&quoted)));
    }
    if !prose.is_empty() {
        parts.push(Part::Text(prose));
    }
    if parts.is_empty() {
        parts.push(Part::Text(String::new()));
    }
    parts
}

/// Break a wide inline object type at its semicolons. Anything else —
/// a plain name, a short type, a union — is returned as it came.
fn wrap_type(t: &str) -> String {
    if t.len() <= WIDE_TYPE || !t.starts_with('{') || !t.ends_with('}') {
        return t.to_string();
    }
    let inner = t[1..t.len() - 1].trim();
    let props: Vec<&str> = inner
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if props.len() < 2 {
        return t.to_string();
    }
    let mut out = String::from("{\n");
    for p in props {
        out.push_str("  ");
        out.push_str(p);
        out.push_str(";\n");
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Part {
        Part::Text(s.into())
    }
    fn quoted(s: &str) -> Part {
        Part::Quoted(s.into())
    }

    /// The common case: one sentence, one name in quotes.
    #[test]
    fn a_plain_message_is_one_row() {
        let r = rows("Cannot redeclare block-scoped variable 'total'.");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].depth, 0);
        assert_eq!(
            r[0].parts,
            vec![
                text("Cannot redeclare block-scoped variable "),
                quoted("total"),
                text("."),
            ]
        );
    }

    /// The suggestion is the useful half, so it gets its own row rather
    /// than trailing off the end of the complaint.
    #[test]
    fn a_suggestion_is_split_off_and_indented() {
        let r = rows("Cannot find name 'totl'. Did you mean 'total'?");
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0].parts,
            vec![text("Cannot find name "), quoted("totl"), text(".")]
        );
        assert_eq!(r[1].depth, 1, "the answer sits under the complaint");
        assert_eq!(
            r[1].parts,
            vec![text("Did you mean "), quoted("total"), text("?")]
        );
    }

    /// TypeScript folds its reasoning into one message, two spaces per
    /// level. Those levels are the whole point of the panel.
    #[test]
    fn the_reasoning_chain_keeps_its_levels() {
        let msg = "Type 'A' is not assignable to type 'B'.\n  \
                   Types of property 'a' are incompatible.\n    \
                   Type 'number' is not assignable to type 'string'.";
        let r = rows(msg);
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().map(|x| x.depth).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(r[2].parts[1], quoted("number"));
    }

    /// A wide object type is unreadable on one line, and a terminal has
    /// nowhere to scroll it to.
    #[test]
    fn a_wide_object_type_is_broken_at_its_semicolons() {
        let r = rows(
            "Type '{ alpha: number; beta: string; gamma: boolean; delta: string }' is not assignable to type 'Big'.",
        );
        let Part::Quoted(t) = &r[0].parts[1] else {
            panic!("the type is quoted: {:?}", r[0].parts);
        };
        assert!(t.starts_with("{\n  alpha: number;\n"), "{t}");
        assert!(t.ends_with("delta: string;\n}"), "{t}");
        assert_eq!(
            r[0].parts[2],
            text(" is not assignable to type "),
            "and the prose around it survives"
        );
    }

    /// A short type is left exactly as the server wrote it.
    #[test]
    fn a_short_type_is_left_alone() {
        let r = rows("Type '{ a: number }' is not assignable to type 'B'.");
        assert_eq!(r[0].parts[1], quoted("{ a: number }"));
    }

    /// An apostrophe in prose is not an opening quote.
    #[test]
    fn an_unclosed_quote_stays_prose() {
        let r = rows("The server doesn't know.");
        assert_eq!(r[0].parts, vec![text("The server doesn't know.")]);
    }
}
