# Loupe as a context provider

Loupe knows something no other process on your machine knows: the exact
lines you are reading and judging right now. An agent in another pane has to
guess at it, and it pays for the guess in grep calls.

`loupe ctl context` publishes that knowledge. Wire it into your agent's
`UserPromptSubmit` hook and every instruction you type carries the file, the
line range, and the diff you are looking at — with no key press.

## What it prints

````
## Loupe — what the user is looking at

Repo: loupe (branch: spike/context-provider)
Mode: local review of uncommitted changes

src/app.rs:1204-1231  (selected, new side)

```
    let retry = ZANZIBAR_RETRY.clone();
```

Held review comments: src/app.rs:1210
Not yet marked viewed: src/ui.rs, src/diff.rs
````

With no loupe open it prints nothing and exits 0, so the hook does nothing.

## Setup

Loupe installs the hook for you. The first-launch wizard offers it, and
this does the same thing at any time:

```sh
loupe ctl install
```

It finds every coding agent on the machine, adds one `UserPromptSubmit`
hook to each, and prints what it did. `loupe ctl uninstall` takes the hook
back out.

Three rules it follows, because the file belongs to you:

- It keeps every hook already in the file, and every other setting.
- It saves the old file beside the new one, as `<name>.loupe.bak`.
- It marks its own hook, so a second run replaces that hook instead of
  adding a second one. Move the loupe binary and re-run it; the new path
  goes in.

Run it once for each machine. It is not per pane and not per session.

### By hand

The installer writes what follows. Read it if you keep your agent's
settings under version control, or if you want the hook in a project file
rather than a global one.

**Claude Code** — in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "loupe ctl context", "timeout": 5 } ] }
    ]
  }
}
```

**Codex** — hooks go in `~/.codex/hooks.json`, in the same shape. Codex
does not read them from `config.toml`. First turn the feature on, in
`~/.codex/config.toml`:

```toml
[features]
hooks = true
```

Then add the hook, in `~/.codex/hooks.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "loupe ctl context --json", "timeout": 5 } ] }
    ]
  }
}
```

Codex reads one JSON object from a hook and ignores plain stdout, so its
hook calls `loupe ctl context --json`. Claude Code reads plain stdout, so
its hook does not. The installer writes the right one for each.

Codex asks you to trust a hook before it runs it. Approve it once. Codex
writes the approval to `config.toml`, under `[hooks.state]`.

### Codex asks before it runs a hook

Codex will not run a hook it has not been shown. Start `codex`
interactively once and approve loupe's hook; the approval is recorded in
`~/.codex/config.toml` under `[hooks.state]` and holds from then on.

Until you do, `codex exec` skips the hook without saying so. The run log
still prints `hook: UserPromptSubmit` for the hooks it *does* trust, so an
untrusted hook looks like a working one. Count the trust entries if a hook
seems to do nothing:

```sh
grep user_prompt_submit ~/.codex/config.toml
```

### Check it

Run `loupe ctl context` in the pane your agent runs in. Output means the
chain works. Silence means no loupe is open on that repository.

Then open loupe on a repository and ask your agent, in another pane, which
file you are looking at. A named file means the whole chain works.

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

- Unix only. The standard library has no unix sockets on Windows, so
  `loupe ctl install` finds nothing to install there. Press `Y` in loupe to
  copy the same block by hand.
- One loupe for each repository. A second one leaves the socket alone and
  starts without a context provider.
- Loupe must run on the same machine as the agent. Over SSH, copy the
  selection with `Y` instead.
- The hook waits about a second and then gives up. A loupe you suspended
  with `Ctrl+Z` cannot answer — `SIGSTOP` stops the socket thread with the
  rest — so the hook prints nothing rather than hold up your prompt.
- Each list names at most 8 files and then says how many it left out. A
  long hunk keeps both ends and says how many lines went missing.
- `loupe ctl install` refuses to write a hook that points inside a Cargo
  build directory, because that path stops working at the next
  `cargo clean`. Run `cargo install --path .` first.
