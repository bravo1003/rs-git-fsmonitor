# rs-git-fsmonitor

A git fsmonitor hook written in Rust.

Forked from [jgavris/rs-git-fsmonitor](https://github.com/jgavris/rs-git-fsmonitor). This fork has diverged (see the [releases](https://github.com/bravo1003/rs-git-fsmonitor/releases)), and the upstream Homebrew and Habitat packages don't ship it.

## Installation

The heavy lifting is done by Meta's [watchman](https://facebook.github.io/watchman/docs/install.html), so install that first.

### Prebuilt binary (Linux x86_64)

Each [release](https://github.com/bravo1003/rs-git-fsmonitor/releases) ships a static binary with no glibc dependency:

```bash
curl -LO https://github.com/bravo1003/rs-git-fsmonitor/releases/latest/download/rs-git-fsmonitor-x86_64-linux-musl
sudo install -m 755 rs-git-fsmonitor-x86_64-linux-musl /usr/local/bin/rs-git-fsmonitor
```

### From source

```bash
cargo install --git https://github.com/bravo1003/rs-git-fsmonitor
```

### Enable it

Run this in each large repository:

```bash
git config core.fsmonitor rs-git-fsmonitor
git config core.untrackedCache true
```

## Purpose

Git 2.16 added support for a `core.fsmonitor` hook to allow an external tool to inform it which files have changed.

https://blog.github.com/2018-04-05-git-217-released/#speeding-up-status-with-watchman

On repositories with many files, this can be a dramatic speedup.

```shell
λ find . -type f | wc -l
   30737
```

Before:

```shell
λ time git status
On branch master
Your branch is up to date with 'origin/master'.

nothing to commit (use -u to show untracked files)

real	0m0.129s
user	0m0.062s
sys	0m0.268s
```

After:

```shell
λ time git status
On branch master
Your branch is up to date with 'origin/master'.

nothing to commit (use -u to show untracked files)

real	0m0.067s
user	0m0.030s
sys	0m0.026s
```

## Notes

- Pair it with `git config core.untrackedCache true`; without it, `git status` still walks the tree looking for untracked files.
- If watchman restarts or loses track of the repo, the hook tells Git that everything changed, so Git rescans once instead of trusting stale data.
- Watchman never reports changes inside directories listed in `ignore_dirs` (in `.watchmanconfig` or the global `watchman.json`). The hook therefore reports any such directory in the worktree as changed on every run, so Git re-checks it itself. That is correct but slower. If a big tracked directory shows up there, take it out of `ignore_dirs` for that repo. A per-repo `.watchmanconfig` replaces the global list. Then run `watchman watch-del <repo>`.
- Even one reported directory costs a big repo something: Git re-reads the top of the tree and rebuilds its file lookup table, about 0.3s on a checkout with a million files. Some repos ignore only folders that Git also ignores and that hold no tracked files, such as build output or downloaded toolchains. Nothing in those folders can show up in `git status`, so you can turn the reports off for that repo with `git config rs-git-fsmonitor.reportIgnoredDirs false`.

## Uninstallation

```
git config --unset core.fsmonitor
```

Then delete the binary (`/usr/local/bin/rs-git-fsmonitor`, or `cargo uninstall rs-git-fsmonitor`).

## License

MIT
