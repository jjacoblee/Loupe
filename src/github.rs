//! GitHub access via the `gh` CLI (uses the user's existing `gh auth login`).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Command;

pub fn run_gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("failed to spawn `gh` — is the GitHub CLI installed and on PATH?")?;
    if !out.status.success() {
        bail!(
            "gh {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// "owner/repo" for the current directory's repository.
pub fn repo_name_with_owner() -> Result<String> {
    #[derive(Deserialize)]
    struct R {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let json = run_gh(&["repo", "view", "--json", "nameWithOwner"])?;
    let r: R = serde_json::from_str(&json).context("parsing `gh repo view` output")?;
    Ok(r.name_with_owner)
}

/// Swap the owner of an "owner/name" repo for the configured upstream org
/// (people who work across GitHub organizations review PRs on the upstream
/// repo, not on their fork/clone's owner).
fn apply_org(name_with_owner: &str, org: Option<&str>) -> String {
    match (org, name_with_owner.split_once('/')) {
        (Some(org), Some((_owner, name))) if !org.trim().is_empty() => {
            format!("{}/{name}", org.trim())
        }
        _ => name_with_owner.to_string(),
    }
}

/// The "owner/repo" every PR operation targets: the current directory's
/// repository, with the owner replaced by `org` when one is configured.
pub fn resolve_repo(org: Option<&str>) -> Result<String> {
    Ok(apply_org(&repo_name_with_owner()?, org))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub head_ref_name: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
}

pub fn list_open_prs(repo: &str) -> Result<Vec<PrSummary>> {
    let json = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--limit",
        "100",
        "--json",
        "number,title,author,headRefName,isDraft,additions,deletions",
    ])?;
    serde_json::from_str(&json).context("parsing `gh pr list` output")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetail {
    /// GraphQL node ID (needed for the viewed-file mutations).
    pub id: String,
    pub number: u64,
    pub title: String,
    pub head_ref_oid: String,
    pub base_ref_oid: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
}

/// The open PR associated with the currently checked-out branch, if any.
/// Without an upstream org this uses `gh pr view` with no selector, which
/// resolves the current branch (including branch↔PR links recorded by
/// `gh pr checkout`). With an org configured, gh needs an explicit selector
/// alongside `--repo`, so the branch name is passed and matched against the
/// upstream repo's PR head refs. Best-effort: any failure (detached HEAD,
/// no PR for the branch, network) yields None.
pub fn pr_for_current_branch(org: Option<&str>) -> Option<u64> {
    #[derive(Deserialize)]
    struct V {
        number: u64,
        state: String,
    }
    let json = match org {
        None => run_gh(&["pr", "view", "--json", "number,state"]).ok()?,
        Some(_) => {
            let repo = resolve_repo(org).ok()?;
            let branch = crate::gitops::current_branch()?;
            run_gh(&[
                "pr",
                "view",
                &branch,
                "--repo",
                &repo,
                "--json",
                "number,state",
            ])
            .ok()?
        }
    };
    let v: V = serde_json::from_str(&json).ok()?;
    (v.state == "OPEN").then_some(v.number)
}

/// A plausible full commit id (SHA-1 or SHA-256 hex). The oids from
/// `gh pr view` are later spliced into `git show <oid>:<path>` and
/// `git merge-base <oid> <oid>`, so anything that is not pure hex must be
/// rejected here — it could otherwise be parsed by git as an option.
fn is_commit_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn pr_detail(repo: &str, number: u64) -> Result<PrDetail> {
    let n = number.to_string();
    let json = run_gh(&[
        "pr",
        "view",
        &n,
        "--repo",
        repo,
        "--json",
        "id,number,title,headRefOid,baseRefOid,baseRefName,headRefName,url",
    ])?;
    let d: PrDetail = serde_json::from_str(&json).context("parsing `gh pr view` output")?;
    if !is_commit_oid(&d.head_ref_oid) || !is_commit_oid(&d.base_ref_oid) {
        bail!("`gh pr view` returned malformed commit ids");
    }
    Ok(d)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChangedFile {
    #[serde(rename = "filename")]
    pub path: String,
    /// added | removed | modified | renamed | copied | changed
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    #[serde(rename = "previous_filename")]
    pub previous: Option<String>,
}

impl ChangedFile {
    pub fn status_char(&self) -> char {
        match self.status.as_str() {
            "added" => 'A',
            "removed" => 'D',
            "renamed" => 'R',
            "copied" => 'C',
            _ => 'M',
        }
    }
    /// Path on the old (base) side of the diff.
    pub fn old_path(&self) -> &str {
        self.previous.as_deref().unwrap_or(&self.path)
    }
}

pub fn changed_files(repo: &str, number: u64) -> Result<Vec<ChangedFile>> {
    let endpoint = format!("repos/{repo}/pulls/{number}/files");
    // One compact JSON object per line (NDJSON) regardless of pagination,
    // and drops the bulky `patch` field we don't need.
    let out = run_gh(&[
        "api",
        "--paginate",
        "--jq",
        ".[] | {filename,status,additions,deletions,previous_filename}",
        &endpoint,
    ])?;
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("parsing PR files list"))
        .collect()
}

/// Paths in the PR the signed-in user has marked as viewed on GitHub.
/// NDJSON via --jq per the pagination rule (never stitch page arrays).
pub fn viewed_files(repo: &str, number: u64) -> Result<HashSet<String>> {
    let (owner, name) = repo
        .split_once('/')
        .with_context(|| format!("repo `{repo}` is not owner/name"))?;
    let query = "query($owner:String!,$name:String!,$number:Int!,$endCursor:String){\
        repository(owner:$owner,name:$name){pullRequest(number:$number){\
        files(first:100,after:$endCursor){pageInfo{hasNextPage endCursor}\
        nodes{path viewerViewedState}}}}}";
    let out = run_gh(&[
        "api",
        "graphql",
        "--paginate",
        "-f",
        &format!("query={query}"),
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("name={name}"),
        "-F",
        &format!("number={number}"),
        "--jq",
        ".data.repository.pullRequest.files.nodes[] | {path,viewerViewedState}",
    ])?;
    #[derive(Deserialize)]
    struct V {
        path: String,
        #[serde(rename = "viewerViewedState")]
        state: String,
    }
    let mut set = HashSet::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let v: V = serde_json::from_str(line).context("parsing viewed-files list")?;
        if v.state == "VIEWED" {
            set.insert(v.path);
        }
    }
    Ok(set)
}

/// Mark or unmark one file as viewed on GitHub (same checkmarks the PR page
/// shows in its file list).
pub fn set_file_viewed(pr_node_id: &str, path: &str, viewed: bool) -> Result<()> {
    let mutation = if viewed {
        "markFileAsViewed"
    } else {
        "unmarkFileAsViewed"
    };
    let query = format!(
        "mutation($id:ID!,$path:String!){{{mutation}(input:{{pullRequestId:$id,path:$path}}){{clientMutationId}}}}"
    );
    run_gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-f",
        &format!("id={pr_node_id}"),
        "-f",
        &format!("path={path}"),
    ])
    .map(|_| ())
}

pub fn checkout_pr(repo: &str, number: u64) -> Result<()> {
    let n = number.to_string();
    run_gh(&["pr", "checkout", &n, "--repo", repo]).map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSide {
    Left,
    Right,
}

impl CommentSide {
    pub fn api(&self) -> &'static str {
        match self {
            CommentSide::Left => "LEFT",
            CommentSide::Right => "RIGHT",
        }
    }
}

/// Post a review comment on a PR diff line (or a multi-line range).
/// `line`/`start_line` are file line numbers on the given side, and must fall
/// within the PR's diff hunks or GitHub will reject the request.
#[allow(clippy::too_many_arguments)]
pub fn post_review_comment(
    repo: &str,
    number: u64,
    commit_id: &str,
    path: &str,
    body: &str,
    side: CommentSide,
    line: usize,
    start_line: Option<usize>,
) -> Result<()> {
    let endpoint = format!("repos/{repo}/pulls/{number}/comments");
    let line_s = line.to_string();
    let mut args: Vec<String> = vec![
        "api".into(),
        "-X".into(),
        "POST".into(),
        endpoint,
        "-f".into(),
        format!("body={body}"),
        "-f".into(),
        format!("commit_id={commit_id}"),
        "-f".into(),
        format!("path={path}"),
        "-F".into(),
        format!("line={line_s}"),
        "-f".into(),
        format!("side={}", side.api()),
    ];
    if let Some(start) = start_line {
        if start != line {
            args.push("-F".into());
            args.push(format!("start_line={start}"));
            args.push("-f".into());
            args.push(format!("start_side={}", side.api()));
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_gh(&arg_refs).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_overrides_owner_only_when_set() {
        assert_eq!(apply_org("jacob/tool", Some("acme")), "acme/tool");
        assert_eq!(apply_org("jacob/tool", Some(" acme ")), "acme/tool");
        assert_eq!(apply_org("jacob/tool", None), "jacob/tool");
        assert_eq!(apply_org("jacob/tool", Some("")), "jacob/tool");
        assert_eq!(apply_org("jacob/tool", Some("  ")), "jacob/tool");
        // Same org as the owner is a no-op in effect.
        assert_eq!(apply_org("acme/tool", Some("acme")), "acme/tool");
    }

    #[test]
    fn commit_oid_validation() {
        assert!(is_commit_oid(&"a1".repeat(20))); // SHA-1
        assert!(is_commit_oid(&"b2".repeat(32))); // SHA-256
        assert!(!is_commit_oid(""));
        assert!(!is_commit_oid("main"));
        assert!(!is_commit_oid("--output=/tmp/x"));
        assert!(!is_commit_oid(&"g".repeat(40))); // non-hex
        assert!(!is_commit_oid(&"a".repeat(39))); // wrong length
    }
}
