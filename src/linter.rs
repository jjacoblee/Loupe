//! Linters: the problems a language server does not report.
//!
//! A language server knows whether the code *compiles*. A linter knows
//! whether it is any good — an unused import, a `==` that should be
//! `===`, a rule the project agreed on. Both are worth seeing, and a
//! reviewer looking at someone else's branch wants the second one most.
//!
//! Loupe runs a linter as a subprocess with the buffer on its standard
//! input, rather than driving a second language server. A linter that
//! reads stdin needs no handshake, no workspace negotiation and no
//! second process kept alive between keystrokes, and it lints what is on
//! screen rather than what is on disk.
//!
//! The output is JSON in one of two shapes, because that is what the
//! linters people actually run emit. Add another with a `[[linter]]`
//! table in the config.

use crate::lsp::Diagnostic;
use serde::Deserialize;
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long a linter gets before loupe gives up on it. A linter is a
/// convenience: one that hangs must cost a missing underline, never a
/// stalled editor.
const LINT_TIMEOUT: Duration = Duration::from_secs(10);

/// The JSON shape a linter's output comes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// `[{ "messages": [{ "ruleId", "severity", "message", "line", … }] }]`
    /// — ESLint, and everything that copied it.
    Eslint,
    /// `[{ "code", "message", "location": { "row", "column" }, … }]` —
    /// Ruff.
    Ruff,
}

/// A linter loupe knows how to run.
#[derive(Debug, Clone)]
pub struct LinterSpec {
    /// What to call it in messages, and what goes in the diagnostic's
    /// `source` so two tools' complaints stay told apart.
    pub name: String,
    pub cmd: String,
    /// Everything before the file name. The stdin flags are added after.
    pub args: Vec<String>,
    /// Lower-case extensions, without the dot.
    pub exts: Vec<String>,
    pub format: Format,
}

/// The linters loupe runs without being asked.
///
/// Two, because these are the two that are near-universal in the
/// languages loupe already drives, and both read stdin and emit stable
/// JSON. Everything else is a `[[linter]]` table away.
fn built_in() -> Vec<LinterSpec> {
    vec![
        LinterSpec {
            name: "eslint".into(),
            cmd: "eslint".into(),
            args: vec!["--format".into(), "json".into()],
            exts: ["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            format: Format::Eslint,
        },
        LinterSpec {
            name: "ruff".into(),
            cmd: "ruff".into(),
            args: vec![
                "check".into(),
                "--output-format".into(),
                "json".into(),
                "--quiet".into(),
            ],
            exts: vec!["py".into(), "pyi".into()],
            format: Format::Ruff,
        },
    ]
}

static CONFIGURED: std::sync::OnceLock<Vec<LinterSpec>> = std::sync::OnceLock::new();

/// Install the linters from the config file, replacing a built-in of the
/// same name and adding the rest. Called once at startup, before any
/// file is opened.
pub fn configure(extra: Vec<LinterSpec>) {
    let mut all = built_in();
    for one in extra {
        match all.iter().position(|b| b.name == one.name) {
            Some(i) => all[i] = one,
            None => all.push(one),
        }
    }
    let _ = CONFIGURED.set(all);
}

pub fn linters() -> &'static [LinterSpec] {
    CONFIGURED.get_or_init(built_in)
}

/// The linter for a file, if loupe knows one and it is installed.
pub fn spec_for(path: &str) -> Option<&'static LinterSpec> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_lowercase();
    linters().iter().find(|l| l.exts.contains(&ext))
}

/// Where a linter's program is: the project's own copy first, then the
/// machine's.
///
/// `node_modules/.bin` matters more here than anywhere else in loupe. A
/// JavaScript project pins its linter and its plugins as dependencies,
/// and the version in the project is the one whose rules the repository
/// agreed on. A globally installed ESLint would run different rules —
/// or, more often, fail to load the project's config at all.
pub fn resolve(root: &Path, cmd: &str) -> Option<std::path::PathBuf> {
    let local = root.join("node_modules/.bin").join(cmd);
    if local.is_file() {
        return Some(local);
    }
    crate::lsp::which(cmd)
}

/// Every linter loupe knows about and whether it is on this machine —
/// the linter half of `loupe --lsp`.
pub fn doctor() -> Vec<(&'static LinterSpec, bool)> {
    linters()
        .iter()
        .map(|l| (l, crate::lsp::which(&l.cmd).is_some()))
        .collect()
}

/// Lint one buffer. **Blocks** — worker threads only.
///
/// `Ok(None)` means there is no linter for this file, or it is not
/// installed: a different thing from "nothing wrong", and the reason the
/// caller must not clear the last set of problems on it.
pub fn lint(root: &Path, path: &str, text: &str) -> anyhow::Result<Option<Vec<Diagnostic>>> {
    let Some(spec) = spec_for(path) else {
        return Ok(None);
    };
    let Some(program) = resolve(root, &spec.cmd) else {
        return Ok(None);
    };
    let abs = root.join(path);
    let mut cmd = Command::new(&program);
    cmd.args(&spec.args);
    match spec.format {
        Format::Eslint => {
            cmd.arg("--stdin").arg("--stdin-filename").arg(&abs);
        }
        Format::Ruff => {
            cmd.arg("--stdin-filename").arg(&abs).arg("-");
        }
    }
    let mut child = cmd
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A linter that stops reading (a parse error, a crash) closes the
        // pipe, and writing to it then raises `EPIPE`. That is its
        // answer, not a failure of ours.
        let _ = stdin.write_all(text.as_bytes());
    }
    let out = wait_with_timeout(child)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    if json.is_empty() {
        // Nothing on stdout and a non-zero exit is the linter failing to
        // run at all — a bad config, usually. Its own words are the only
        // useful message.
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            let first = said.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            anyhow::bail!("{} could not run: {}", spec.name, first.trim());
        }
        return Ok(Some(Vec::new()));
    }
    let value: Value = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("{} printed something that is not JSON: {e}", spec.name))?;
    Ok(Some(parse(&value, spec)))
}

/// Wait for a child, killing it if it outstays [`LINT_TIMEOUT`].
fn wait_with_timeout(mut child: std::process::Child) -> anyhow::Result<std::process::Output> {
    let deadline = std::time::Instant::now() + LINT_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(child.wait_with_output()?),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                anyhow::bail!("the linter did not finish in time");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Turn a linter's JSON into diagnostics.
pub fn parse(value: &Value, spec: &LinterSpec) -> Vec<Diagnostic> {
    match spec.format {
        Format::Eslint => parse_eslint(value, &spec.name),
        Format::Ruff => parse_ruff(value, &spec.name),
    }
}

fn parse_eslint(value: &Value, source: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let files = match value {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    for file in files {
        let Some(messages) = file.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for m in messages {
            let Some(message) = m.get("message").and_then(Value::as_str) else {
                continue;
            };
            let line = m.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
            let col = m.get("column").and_then(Value::as_u64).unwrap_or(1) as usize;
            let end_line = m
                .get("endLine")
                .and_then(Value::as_u64)
                .unwrap_or(line as u64) as usize;
            let end_col = m.get("endColumn").and_then(Value::as_u64);
            out.push(Diagnostic {
                line,
                col,
                // A problem that runs past its own line is marked on the
                // first one, the way the language server's are.
                end_col: match end_col {
                    Some(c) if end_line == line => c as usize,
                    Some(_) => usize::MAX,
                    None => col + 1,
                },
                // ESLint counts the other way round from LSP: 2 is its
                // error and 1 its warning. Getting this backwards would
                // paint every warning red and every error yellow.
                severity: match m.get("severity").and_then(Value::as_u64) {
                    Some(2) => 1,
                    _ => 2,
                },
                message: message.trim().to_string(),
                code: m.get("ruleId").and_then(Value::as_str).map(str::to_string),
                source: Some(source.to_string()),
            });
        }
    }
    out
}

fn parse_ruff(value: &Value, source: &str) -> Vec<Diagnostic> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| {
            let message = m.get("message").and_then(Value::as_str)?;
            let line = m.pointer("/location/row").and_then(Value::as_u64)? as usize;
            let col = m.pointer("/location/column").and_then(Value::as_u64)? as usize;
            let end_line = m.pointer("/end_location/row").and_then(Value::as_u64);
            let end_col = m.pointer("/end_location/column").and_then(Value::as_u64);
            Some(Diagnostic {
                line,
                col,
                end_col: match (end_line, end_col) {
                    (Some(l), Some(c)) if l as usize == line => c as usize,
                    (Some(_), Some(_)) => usize::MAX,
                    _ => col + 1,
                },
                // Ruff reports no severity: everything it finds is a rule
                // the project chose to turn on, which is a warning here.
                // Calling it an error would outrank the compiler.
                severity: 2,
                message: message.trim().to_string(),
                code: m.get("code").and_then(Value::as_str).map(str::to_string),
                source: Some(source.to_string()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eslint_spec() -> LinterSpec {
        LinterSpec {
            name: "eslint".into(),
            cmd: "eslint".into(),
            args: Vec::new(),
            exts: vec!["ts".into()],
            format: Format::Eslint,
        }
    }

    /// ESLint numbers its severities the other way round from LSP. A
    /// warning painted red is a warning people stop believing, so this is
    /// the one thing in the parser that must not be guessed.
    #[test]
    fn eslint_severities_are_turned_the_right_way_round() {
        let v = json!([{
            "filePath": "/repo/a.ts",
            "messages": [
                {"ruleId": "no-undef", "severity": 2, "message": "'x' is not defined.",
                 "line": 3, "column": 5, "endLine": 3, "endColumn": 6},
                {"ruleId": "no-unused-vars", "severity": 1, "message": "'y' is assigned a value but never used.",
                 "line": 4, "column": 7, "endLine": 4, "endColumn": 8}
            ]
        }]);
        let out = parse(&v, &eslint_spec());
        assert_eq!(out.len(), 2);
        assert!(out[0].is_error(), "ESLint's 2 is an error");
        assert_eq!(out[0].code.as_deref(), Some("no-undef"));
        assert_eq!(out[0].source.as_deref(), Some("eslint"));
        assert_eq!(out[0].code_label().as_deref(), Some("eslint(no-undef)"));
        assert!(out[1].is_warning(), "and its 1 is a warning");
        assert_eq!(out[1].line, 4);
        assert_eq!((out[1].col, out[1].end_col), (7, 8));
    }

    /// A problem that runs past its own line is marked on the first one,
    /// the way the language server's are.
    #[test]
    fn a_span_over_several_lines_is_marked_on_the_first() {
        let v = json!([{"messages": [
            {"ruleId": "block", "severity": 2, "message": "unclosed",
             "line": 2, "column": 1, "endLine": 9, "endColumn": 3}
        ]}]);
        let out = parse(&v, &eslint_spec());
        assert_eq!(out[0].line, 2);
        assert_eq!(out[0].end_col, usize::MAX);
    }

    /// A clean file is an empty list, not an error.
    #[test]
    fn a_clean_file_lints_to_nothing() {
        let v = json!([{"filePath": "/repo/a.ts", "messages": []}]);
        assert!(parse(&v, &eslint_spec()).is_empty());
    }

    /// Ruff's shape, and its one severity.
    #[test]
    fn ruff_findings_are_warnings_with_their_rule_code() {
        let spec = LinterSpec {
            name: "ruff".into(),
            cmd: "ruff".into(),
            args: Vec::new(),
            exts: vec!["py".into()],
            format: Format::Ruff,
        };
        let v = json!([{
            "code": "F401",
            "message": "`os` imported but unused",
            "location": {"row": 1, "column": 8},
            "end_location": {"row": 1, "column": 10}
        }]);
        let out = parse(&v, &spec);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].is_warning(),
            "a lint rule does not outrank the compiler"
        );
        assert_eq!(out[0].code_label().as_deref(), Some("ruff(F401)"));
        assert_eq!((out[0].line, out[0].col, out[0].end_col), (1, 8, 10));
    }

    /// A JavaScript project pins its linter and its plugins. The copy in
    /// `node_modules/.bin` is the one whose rules the repository agreed
    /// on, so it wins over anything installed globally.
    #[test]
    fn the_projects_own_linter_wins() {
        let root = std::env::temp_dir().join(format!("loupe-lint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
        let local = root.join("node_modules/.bin/eslint");
        std::fs::write(&local, "#!/bin/sh\n").unwrap();

        assert_eq!(resolve(&root, "eslint"), Some(local));
        assert_eq!(
            resolve(&root, "definitely-not-a-real-linter-xyz"),
            None,
            "and a linter nobody has is nowhere"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The extension picks the linter, and an unknown one picks none.
    #[test]
    fn the_extension_picks_the_linter() {
        assert_eq!(
            spec_for("src/a.ts").map(|l| l.name.as_str()),
            Some("eslint")
        );
        assert_eq!(
            spec_for("src/a.jsx").map(|l| l.name.as_str()),
            Some("eslint")
        );
        assert_eq!(spec_for("main.py").map(|l| l.name.as_str()), Some("ruff"));
        assert_eq!(spec_for("src/main.rs").map(|l| l.name.as_str()), None);
        assert_eq!(spec_for("README").map(|l| l.name.as_str()), None);
    }
}
