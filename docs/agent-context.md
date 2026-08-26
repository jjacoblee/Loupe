# Loupe as a context provider

Loupe knows something no other process on your machine knows: the exact
lines you are reading and judging right now. An agent in another pane has to
guess at it, and it pays for the guess in grep calls.

`loupe ctl context` publishes that knowledge. Wire it into your agent's
`UserPromptSubmit` hook and every instruction you type carries the file, the
line range, and the diff you are looking at — with no key press.

## What it prints

```
## Loupe — what the user is looking at

Repo: loupe (branch: spike/context-provider)
Mode: local review of uncommitted changes

src/app.rs:1204-1231  (selected, new side)

```
    let retry = ZANZIBAR_RETRY.clone();
```

Held review comments: src/app.rs:1210
Not yet marked viewed: src/ui.rs, src/diff.rs
```

With no loupe open it prints nothing and exits 0, so the hook does nothing.

## Setup

Run it once per machine. It is not per pane and not per session.

### Claude Code

In `~/.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "loupe ctl context", "timeout": 5 } ] }
    ]
  }
}
```

### Codex

In `~/.codex/config.toml`:

```toml
[[hooks.UserPromptSubmit]]

[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "loupe ctl context"
timeout = 5
```

Codex asks you to trust a hook before it runs it. Approve it once.

### Check it

Run `loupe ctl context` in the pane your agent runs in. Output means the
chain works. Silence means no loupe is open on that repository.

## How the two panes find each other

They do not. **The repository root is the address.**

The hook does not run in loupe's pane. Your agent runs it as a child
process, in your agent's own working directory. That command resolves the
git repository root, derives the socket path from it, and connects.

1. You press Enter in the agent pane.
2. The agent runs `loupe ctl context` as a child.
3. That child finds the repository root and connects to loupe's socket.
4. Loupe answers with what is on screen.
5. The agent gives the answer to the model.

So tmux, terminal splits, and separate windows all behave the same, because
there is no pane to find. The two requirements are that loupe and the agent
run on the same machine and sit in the same repository.

Nothing is typed into your prompt. Your prompt stays as you wrote it; the
context arrives beside it as a system message.

## Limits

- Unix only. The standard library has no unix sockets on Windows.
- One loupe for each repository. A second one leaves the socket alone and
  starts without a context provider.
- Loupe must run on the same machine as the agent. Over SSH, copy the
  selection with `y` instead.
