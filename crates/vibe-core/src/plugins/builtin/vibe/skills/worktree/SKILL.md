---
name: worktree
description: Manage the git worktrees Vibe keeps in $VIBE_HOME/worktrees, whether making one for a task, picking an existing one back up, or listing and deleting them. Load it when the user asks for an isolated checkout or wants to tidy the ones already there.
user-invocable: true
---

# Git worktrees in Vibe

Vibe keeps every worktree it manages below `$VIBE_HOME/worktrees/`, which is
`~/.vibe/worktrees/` unless `VIBE_HOME` moves it. Each repository gets its own
bucket, named after the repository root's directory and the first twelve hex
digits of the SHA-256 of its common git directory, so two clones with the same
folder name never share a bucket.

| Path | Holds |
|---|---|
| `<bucket>/<name>/` | the checkout itself |
| `.claims/<bucket>/<name>/record.json` | branch, base commit, whether the branch was created, claim time |
| `.claims/<bucket>/<name>/recovery.json` | what a retained chat needs to rebuild its checkout |
| `.claims/<bucket>/<name>/holders/` | one empty marker per session using the worktree |

## Making a worktree

- Work out the bucket first, from the repository root and its common git dir.
- The name must be one portable path segment: none of `<>:"/\|?*`, no trailing
  dot or space, not `.` or `..`, and none of the Windows device names (`CON`,
  `PRN`, `AUX`, `NUL`, `COM1` to `COM9`, `LPT1` to `LPT9`).
- Claim the directory with a plain `mkdir`. It is atomic, and it is the only
  way to tell a taken path from a bad ref, since `git worktree add` exits 128
  for both.
- A new branch starts from the remote's default branch, fetched first when the
  network allows; it never starts from whatever the current checkout has at
  `HEAD`. An existing branch is checked out as it is.
- Run `git worktree add -b <name> <path> <start>` for a new branch, or drop
  `-b` to reuse one.
- Record the new worktree's own `HEAD` as `base_commit`: cleanup measures
  against it, not against the checkout Vibe was started from.
- Write `record.json` into the claim directory.

Without a name, build one from the request: NFKD-normalize it, lowercase it,
turn every run of characters outside `[a-z0-9]` into `-`, keep six words at
most and 40 characters at most, and strip stop words left at the end. The
branch takes a `vibe/` prefix. On a clash append `-2`, `-3` and so on, giving up
after 100; an empty result falls back to a random slug.

Pick an existing worktree back up only when its path has no symbolic link in
it, its `.git` is a file rather than a directory, it shares the repository's
common git dir, and it is on the expected branch.

## Holders

A session working in a worktree registers a holder marker and keeps it locked
for as long as it runs, which is what stops background cleanup from deleting
the checkout underneath it. A marker nobody holds a lock on was left by a
process that died, and may be deleted. While a session is still attaching, a
temporary holder covers the gap.

## Deleting a worktree

Look at its state before anything else:

```sh
git -c core.fsmonitor= status --porcelain --untracked-files=all
git rev-list --count <base_commit>..HEAD
```

Modified files, untracked files and commits beyond `base_commit` each make the
worktree dirty, and deleting it throws all three away, so ask the user first.
A branch the session did not create needs its own confirmation before it is
deleted. If any other session still holds the worktree, leave it alone. Change
directory out of it before removing it, since Windows will not delete a
process's working directory.

Then run `git worktree remove --force <path>`, and `git branch -D <name>` when
the branch was created here or the user agreed. Remove the claim record and
`rmdir` the directories that are now empty; never delete them recursively, or
a holder that appeared meanwhile would be lost.

## Snapshots and retention

Vibe keeps the newest configured number of managed worktrees across all
repositories and never touches one in use. An inactive worktree beyond that
count is first saved: everything in it, untracked files included and ignored
ones excluded, is committed through a separate index file to
`refs/vibe/reaped/<name>`, leaving the worktree's own index as it was. If the
snapshot fails the worktree stays. Its `recovery.json` outlives the claim, so
reopening the chat later rebuilds the checkout on its own. A reservation that
never got a `base_commit` is discarded only when it is empty and no creating
process still holds its marker.

Creating, attaching and retention all take one lock shared across processes,
so a worktree cannot vanish while another process is claiming it.

## Worth remembering

- Pass `-c core.fsmonitor=` whenever reading a working tree another tool may
  watch.
- Paths through symbolic links are refused.
- A worktree is trusted for the current session only; nothing is persisted.
