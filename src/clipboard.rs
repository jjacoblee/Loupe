//! Copying text out of Loupe.
//!
//! Loupe turns on mouse reporting, which is what makes files, folds and
//! diff lines clickable — and which also takes the terminal's own
//! click-drag text selection away. (Most terminals give it back while a
//! modifier is held: Option on macOS Terminal and iTerm2, Shift on most
//! others.) That covers "grab whatever is on screen", but not "copy the
//! seven lines this PR deleted", which is the thing a reviewer actually
//! wants — so Loupe copies selections itself.
//!
//! Two mechanisms, tried in order:
//!
//! 1. **A clipboard command** — `pbcopy`, `wl-copy`, `xclip`, `xsel`,
//!    `clip.exe`. Reliable, and consistent with the rest of Loupe, which
//!    shells out to real tools rather than reimplementing them.
//! 2. **OSC 52** — an escape sequence asking the *terminal* to set the
//!    clipboard, which is the only thing that works over SSH, where the
//!    commands above would set the clipboard of the wrong machine.
//!
//! No clipboard crate: the fallback needs base64 and nothing else, and
//! that is twenty lines.

use anyhow::{bail, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Commands that read the text to copy from stdin, in the order they are
/// tried. First one that exists and exits cleanly wins.
const COMMANDS: &[(&str, &[&str])] = &[
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("clip.exe", &[]),
];

/// Terminals cap how much they will accept in one OSC 52 sequence, and an
/// over-long one is dropped silently — worse than an error. This is
/// comfortably under the common limits.
const OSC52_MAX: usize = 74_000;

/// Copy `text` to the system clipboard. Returns the mechanism that
/// worked, so the status line can say where the text went — on a remote
/// machine "copied" is ambiguous otherwise.
pub fn copy(text: &str) -> Result<&'static str> {
    let mut last_err = None;
    for (cmd, args) in COMMANDS {
        match run(cmd, args, text) {
            Ok(()) => return Ok(cmd),
            // Not installed: try the next one, and don't report it.
            Err(e) if is_missing(&e) => {}
            Err(e) => last_err = Some(e),
        }
    }
    match osc52(text) {
        Ok(()) => Ok("the terminal"),
        Err(e) => match last_err {
            Some(first) => Err(first),
            None => Err(e),
        },
    }
}

fn is_missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn run(cmd: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Dropping stdin closes the pipe, which is what tells the command it
    // has the whole input — without it `pbcopy` waits forever.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{cmd} exited with {status}");
    }
    Ok(())
}

/// Ask the terminal itself to set the clipboard. Written to `/dev/tty`
/// rather than stdout so it can't be captured or reordered by whatever
/// ratatui is drawing.
fn osc52(text: &str) -> Result<()> {
    if text.len() > OSC52_MAX {
        bail!(
            "too much text to copy through the terminal ({} KB) — install pbcopy/xclip/wl-copy",
            text.len() / 1024
        );
    }
    let payload = base64(text.as_bytes());
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    write!(tty, "\x1b]52;c;{payload}\x07")?;
    tty.flush()?;
    Ok(())
}

/// Standard base64, no line breaks. (Only used for OSC 52.)
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_examples() {
        // RFC 4648 §10, which exists precisely to catch padding mistakes.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_high_bytes_and_utf8() {
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64("é→".as_bytes()), "w6nihpI=");
    }

    #[test]
    fn oversized_text_is_refused_by_the_terminal_path() {
        let big = "x".repeat(OSC52_MAX + 1);
        let err = osc52(&big).unwrap_err().to_string();
        assert!(err.contains("too much text"), "{err}");
    }
}
