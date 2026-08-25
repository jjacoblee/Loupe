//! GitHub access via the `gh` CLI (uses the user's existing `gh auth login`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

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

/// The same, with a request body on standard input.
///
/// A review carries an array of comment objects, and `gh api -f key=value`
/// can only express flat strings — so the body goes in as JSON through
/// `--input -` instead.
fn run_gh_stdin(args: &[&str], input: &str) -> Result<String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `gh` — is the GitHub CLI installed and on PATH?")?;
    child
        .stdin
        .take()
        .context("gh took no stdin")?
        .write_all(input.as_bytes())
        .context("writing the request body to gh")?;
    let out = child.wait_with_output().context("waiting for gh")?;
    if !out.status.success() {
        // `gh api` writes the API's JSON to stdout even when it fails, and
        // only a one-line summary to stderr — so the sentence worth
        // showing is usually on the *successful* stream.
        bail!(
            "{}",
            gh_message(
                &String::from_utf8_lossy(&out.stdout),
                &String::from_utf8_lossy(&out.stderr),
            )
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The sentence worth showing out of a `gh api` failure.
///
/// The API's JSON carries the only part a reader can act on ("Can not
/// approve your own pull request", or which comment fell outside the
/// diff), and gh writes it to stdout while stderr gets "gh: Unprocessable
/// Entity (HTTP 422)". So both streams are searched, and the raw text is
/// the fallback when neither holds JSON.
fn gh_message(stdout: &str, stderr: &str) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        message: Option<String>,
        errors: Option<Vec<ApiSubError>>,
    }
    #[derive(Deserialize)]
    struct ApiSubError {
        message: Option<String>,
        field: Option<String>,
    }
    let parse = |text: &str| {
        text.find('{')
            .and_then(|i| serde_json::from_str::<ApiError>(text[i..].trim()).ok())
            .filter(|e| e.message.is_some())
    };
    match parse(stdout).or_else(|| parse(stderr)) {
        Some(e) => {
            let head = e.message.unwrap_or_default();
            // The per-field errors say which comment GitHub refused and
            // why — usually a line outside the diff.
            let detail: Vec<String> = e
                .errors
                .unwrap_or_default()
                .into_iter()
                .filter_map(|d| match (d.field, d.message) {
                    (Some(f), Some(m)) => Some(format!("{f}: {m}")),
                    (None, Some(m)) => Some(m),
                    _ => None,
                })
                .collect();
            if detail.is_empty() {
                head
            } else {
                format!("{head} ({})", detail.join("; "))
            }
        }
        // Nothing parseable: whichever stream actually said something.
        None => {
            let e = stderr.trim();
            if e.is_empty() {
                stdout.trim().to_string()
            } else {
                e.to_string()
            }
        }
    }
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
    /// Web URL of the PR, as `gh pr view --json url` reports it. Right-click
    /// on the PR badge copies this, so the link works on GitHub Enterprise
    /// too, where github.com is the wrong host.
    #[serde(default)]
    pub url: String,
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
    /// A merge left this path unmerged in the working tree. Local review
    /// only — GitHub's file list never says this, so it deserializes to
    /// false and the local scan fills it in (see `gitops::local_changes`).
    #[serde(default, skip)]
    pub conflicted: bool,
}

impl ChangedFile {
    pub fn status_char(&self) -> char {
        if self.conflicted {
            return '!';
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

// ----------------------------------------------------------------- reviews

/// What submitting a review says about the pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    /// Notes, no judgement. GitHub's `COMMENT`.
    #[default]
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    /// The `event` value the API takes.
    pub fn api(self) -> &'static str {
        match self {
            Verdict::Comment => "COMMENT",
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// The button label.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Comment => "Comment",
            Verdict::Approve => "Approve",
            Verdict::RequestChanges => "Request changes",
        }
    }

    /// The mark drawn beside the label, so the three read apart at a
    /// glance rather than by their words.
    pub fn icon(self) -> &'static str {
        match self {
            Verdict::Comment => "💬",
            Verdict::Approve => "✓",
            Verdict::RequestChanges => "✕",
        }
    }

    /// What it did, for the status line after it lands.
    pub fn past(self) -> &'static str {
        match self {
            Verdict::Comment => "commented on",
            Verdict::Approve => "approved",
            Verdict::RequestChanges => "requested changes on",
        }
    }

    /// The three, in the order the dropdown lists them.
    pub fn all() -> [Verdict; 3] {
        [Verdict::Comment, Verdict::Approve, Verdict::RequestChanges]
    }
}

/// One inline comment of a review: where it goes, and what it says.
///
/// Held comments are written to disk between runs, so this is the on-disk
/// shape as well as the request shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub path: String,
    pub side: CommentSide,
    /// Last line of the range, 1-based, on `side`.
    pub line: usize,
    /// First line of a multi-line range. `None` for a single line.
    pub start_line: Option<usize>,
    pub body: String,
}

impl ReviewComment {
    /// How the range reads in a list: "src/app.rs:12" or "src/app.rs:12–18".
    pub fn where_at(&self) -> String {
        match self.start_line.filter(|s| *s != self.line) {
            Some(start) => format!("{}:{start}–{}", self.path, self.line),
            None => format!("{}:{}", self.path, self.line),
        }
    }

    /// The request object GitHub takes for one comment.
    fn json(&self) -> serde_json::Value {
        let mut o = serde_json::json!({
            "path": self.path,
            "line": self.line,
            "side": self.side.api(),
            "body": self.body,
        });
        // A single-line comment must NOT carry start_line: GitHub rejects a
        // range whose two ends are the same line.
        if let Some(start) = self.start_line.filter(|s| *s != self.line) {
            o["start_line"] = start.into();
            o["start_side"] = self.side.api().into();
        }
        o
    }
}

/// The request body for one review.
///
/// An empty field is left out rather than sent empty: GitHub reads a
/// present-but-blank `body` as a body, and an empty `comments` array as an
/// attempt to review nothing.
fn review_payload(
    commit_id: &str,
    body: &str,
    verdict: Verdict,
    comments: &[ReviewComment],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "commit_id": commit_id,
        "event": verdict.api(),
    });
    if !body.trim().is_empty() {
        payload["body"] = body.into();
    }
    if !comments.is_empty() {
        payload["comments"] = comments.iter().map(ReviewComment::json).collect();
    }
    payload
}

/// Submit one review: a body, a verdict, and every inline comment at once.
///
/// This is the whole point of holding comments back. Posting them one at a
/// time notifies everyone watching the pull request once per comment and
/// leaves no summary tying them together; one review is a single
/// notification with the notes attached to it.
///
/// `commit_id` anchors the inline comments — they must fall on lines that
/// commit's diff actually touches, or GitHub rejects the whole review.
pub fn submit_review(
    repo: &str,
    number: u64,
    commit_id: &str,
    body: &str,
    verdict: Verdict,
    comments: &[ReviewComment],
) -> Result<()> {
    let payload = review_payload(commit_id, body, verdict, comments);
    let endpoint = format!("repos/{repo}/pulls/{number}/reviews");
    run_gh_stdin(
        &["api", "-X", "POST", &endpoint, "--input", "-"],
        &payload.to_string(),
    )
    .map(|_| ())
}

// ------------------------------------------------------------------ blame

/// The pull request a blamed commit belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub number: u64,
    pub title: String,
    pub url: String,
}

/// Commit hashes asked about in one GraphQL call. GitHub caps a query's
/// node count, and a batch this size keeps the request comfortably small
/// while still costing one call for a typical file's history.
const PR_BATCH: usize = 50;

/// Which pull request each of `shas` was merged in, for the blame pane.
///
/// One GraphQL query per batch, with the commits as aliased `object(oid:)`
/// fields — the REST route would be one request per commit. Commits with
/// no associated pull request are simply absent from the result, as are
/// hashes GitHub does not know; neither is an error, because a repository
/// full of direct pushes is a normal repository.
pub fn pulls_for_commits(
    repo: &str,
    shas: &[String],
) -> Result<std::collections::HashMap<String, PrRef>> {
    let (owner, name) = repo
        .split_once('/')
        .context("repository must be owner/name")?;
    let mut out = std::collections::HashMap::new();
    for batch in shas.chunks(PR_BATCH) {
        let mut fields = String::new();
        for (i, sha) in batch.iter().enumerate() {
            // Hashes come from `git blame`, but they are spliced into a
            // query string, so anything that is not hex is dropped rather
            // than escaped.
            if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            fields.push_str(&format!(
                "c{i}: object(oid: \"{sha}\") {{ ... on Commit {{ oid \
                 associatedPullRequests(first: 1) {{ nodes {{ number title url }} }} }} }} "
            ));
        }
        if fields.is_empty() {
            continue;
        }
        let query = format!(
            "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}"
        );
        let json = run_gh(&[
            "api",
            "graphql",
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
            "-f",
            &format!("query={query}"),
        ])?;
        merge_pr_nodes(&json, &mut out)?;
    }
    Ok(out)
}

/// Pull the `oid → pull request` pairs out of one GraphQL response.
/// Split out so the shape can be tested without a network call.
fn merge_pr_nodes(json: &str, out: &mut std::collections::HashMap<String, PrRef>) -> Result<()> {
    #[derive(Deserialize)]
    struct Resp {
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<std::collections::HashMap<String, Option<Obj>>>,
    }
    #[derive(Deserialize)]
    struct Obj {
        oid: Option<String>,
        #[serde(rename = "associatedPullRequests")]
        prs: Option<Nodes>,
    }
    #[derive(Deserialize)]
    struct Nodes {
        nodes: Vec<Node>,
    }
    #[derive(Deserialize)]
    struct Node {
        number: u64,
        title: String,
        url: String,
    }
    let resp: Resp = serde_json::from_str(json).context("could not read the GraphQL reply")?;
    let Some(repo) = resp.data.and_then(|d| d.repository) else {
        return Ok(());
    };
    for obj in repo.into_values().flatten() {
        let (Some(oid), Some(nodes)) = (obj.oid, obj.prs) else {
            continue;
        };
        if let Some(n) = nodes.nodes.into_iter().next() {
            out.insert(
                oid,
                PrRef {
                    number: n.number,
                    title: n.title,
                    url: n.url,
                },
            );
        }
    }
    Ok(())
}

/// Open a pull request in the reader's browser. `gh` picks the right host,
/// which matters on GitHub Enterprise, and it is already the only outward
/// route loupe has.
pub fn open_pr_web(repo: &str, number: u64) -> Result<()> {
    let n = number.to_string();
    run_gh(&["pr", "view", &n, "--repo", repo, "--web"]).map(|_| ())
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

    /// The GraphQL reply for the blame pane: aliased commit objects, one
    /// of which has no pull request behind it and one of which GitHub
    /// does not know at all.
    #[test]
    fn pull_request_nodes_are_read_out_of_the_graphql_reply() {
        let json = r#"{"data":{"repository":{
            "c0":{"oid":"aaa1","associatedPullRequests":{"nodes":[
                {"number":412,"title":"Fix the parser","url":"https://gh/x/pull/412"}]}},
            "c1":{"oid":"bbb2","associatedPullRequests":{"nodes":[]}},
            "c2":null}}}"#;
        let mut out = std::collections::HashMap::new();
        merge_pr_nodes(json, &mut out).expect("valid reply");
        assert_eq!(out.len(), 1, "only the commit with a pull request lands");
        let pr = &out["aaa1"];
        assert_eq!(pr.number, 412);
        assert_eq!(pr.title, "Fix the parser");
        assert_eq!(pr.url, "https://gh/x/pull/412");
    }

    /// A reply carrying only errors must not panic or invent a pull
    /// request — the pane simply shows no link for those commits.
    #[test]
    fn a_graphql_error_reply_yields_nothing() {
        let mut out = std::collections::HashMap::new();
        merge_pr_nodes(r#"{"errors":[{"message":"nope"}]}"#, &mut out).expect("no panic");
        assert!(out.is_empty());
        assert!(merge_pr_nodes("not json", &mut out).is_err());
    }

    // --------------------------------------------------------- reviews

    fn rc(path: &str, line: usize, start: Option<usize>) -> ReviewComment {
        ReviewComment {
            path: path.into(),
            side: CommentSide::Right,
            line,
            start_line: start,
            body: "note".into(),
        }
    }

    /// One review carries the summary, the verdict, and every comment.
    #[test]
    fn a_review_goes_up_as_one_request() {
        let comments = [rc("a.rs", 12, None), rc("b.rs", 20, Some(15))];
        let v = review_payload(
            "c".repeat(40).as_str(),
            "looks good",
            Verdict::Approve,
            &comments,
        );
        assert_eq!(v["event"], "APPROVE");
        assert_eq!(v["body"], "looks good");
        assert_eq!(v["commit_id"], "c".repeat(40));
        let cs = v["comments"].as_array().expect("an array");
        assert_eq!(cs.len(), 2, "both comments ride along");
        // A single line must NOT carry start_line — GitHub rejects a range
        // whose two ends are the same line.
        assert_eq!(cs[0]["line"], 12);
        assert_eq!(cs[0]["side"], "RIGHT");
        assert!(cs[0].get("start_line").is_none());
        // A range carries both ends, and a side for each.
        assert_eq!(cs[1]["start_line"], 15);
        assert_eq!(cs[1]["line"], 20);
        assert_eq!(cs[1]["start_side"], "RIGHT");
    }

    /// An empty field is left out rather than sent blank.
    #[test]
    fn empty_halves_of_a_review_are_omitted() {
        let none: [ReviewComment; 0] = [];
        let v = review_payload("abc", "  ", Verdict::Comment, &none);
        assert!(v.get("body").is_none(), "a blank summary is not a summary");
        assert!(v.get("comments").is_none(), "nor is an empty list a list");
        assert_eq!(v["event"], "COMMENT");

        // A range whose ends match is one line, however it was stored.
        let same = [rc("a.rs", 7, Some(7))];
        let v = review_payload("abc", "x", Verdict::RequestChanges, &same);
        assert_eq!(v["event"], "REQUEST_CHANGES");
        assert!(v["comments"][0].get("start_line").is_none());
    }

    /// A rejected review has to say why, and gh splits that across two
    /// streams: the API's JSON on stdout, a bare status line on stderr.
    #[test]
    fn the_api_error_is_pulled_out_of_ghs_noise() {
        // The real shape, as `gh api` emits it.
        let out = r#"{"message":"Can not approve your own pull request","documentation_url":"https://docs.github.com"}"#;
        let err = "gh: Unprocessable Entity (HTTP 422)";
        assert_eq!(
            gh_message(out, err),
            "Can not approve your own pull request",
            "the useful half is on stdout"
        );

        // Per-field errors name the comment GitHub refused.
        let out = r#"{"message":"Validation Failed","errors":[{"field":"line","message":"must be part of the diff"}]}"#;
        assert_eq!(
            gh_message(out, "gh: HTTP 422"),
            "Validation Failed (line: must be part of the diff)"
        );

        // JSON on stderr instead still works.
        assert_eq!(
            gh_message("", r#"gh: HTTP 404 {"message":"Not Found"}"#),
            "Not Found"
        );

        // Nothing to parse: whichever stream said something.
        assert_eq!(
            gh_message("", "  connection refused\n"),
            "connection refused"
        );
        assert_eq!(gh_message("odd output\n", ""), "odd output");
    }

    /// The whole request path, against the real `gh`: the JSON is built,
    /// written to gh's stdin, sent, and the refusal is turned back into a
    /// sentence. The repository does not exist, so nothing can be created
    /// — a 404 is the expected answer and the point of the test.
    ///
    /// Ignored by default: it needs `gh auth login` and the network.
    /// Run it with `cargo test -- --ignored real_gh`.
    #[test]
    #[ignore = "needs gh auth and the network"]
    fn real_gh_carries_the_review_and_reports_the_refusal() {
        let comments = [rc("a.rs", 1, None)];
        let err = submit_review(
            "jjacoblee/loupe-nonexistent-probe-9z8x7",
            1,
            &"0".repeat(40),
            "probe",
            Verdict::Comment,
            &comments,
        )
        .expect_err("a repository that does not exist cannot take a review");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Not Found"),
            "the API's own words reach the reader, not gh's exit code: {msg}"
        );
        assert!(!msg.contains("HTTP"), "and without the status noise: {msg}");
    }

    /// Where a comment lands, as the confirm prompt spells it.
    #[test]
    fn a_comment_says_where_it_goes() {
        assert_eq!(rc("src/a.rs", 12, None).where_at(), "src/a.rs:12");
        assert_eq!(rc("src/a.rs", 18, Some(12)).where_at(), "src/a.rs:12–18");
        assert_eq!(rc("src/a.rs", 9, Some(9)).where_at(), "src/a.rs:9");
    }
}
