# Fork workflow (read before working)

This repository is a personal fork of Zed (`origin` = `MichaelT025/zed`,
`upstream` = `zed-industries/zed`). All work happens in **one worktree**, one
feature branch at a time.

## Branches

| Branch        | Purpose                                                              |
|---------------|----------------------------------------------------------------------|
| `main`        | Exact mirror of `upstream/main`. No fork commits, ever.              |
| `integration` | The fork itself: `main` plus this file and every finished fork feature. Long-lived; the GitHub default branch. |
| `feature/*`   | One branch per feature in progress, created from `integration`.      |

Changes only flow out of `main`, never into it:

```
upstream/main ──ff──▶ main ──merge──▶ integration ◀──merge── feature/<name>
```

`git diff main...integration` shows everything the fork changes on top of
upstream.

## Working on a feature

1. Start from an up-to-date `integration`:
   ```sh
   git checkout integration
   git checkout -b feature/<name>
   ```
2. Commit the feature work on `feature/<name>`. Don't commit feature code
   directly on `integration`.
3. When it's done and `cargo check -p zed` passes, merge it back and delete the
   branch:
   ```sh
   git checkout integration
   git merge feature/<name>
   git branch -d feature/<name>
   ```

Keep features small and sequential. Because the fork builds on top of itself,
a new feature can rely on any feature already merged into `integration`.

## Updating from upstream

```sh
git checkout main && git fetch upstream && git merge --ff-only upstream/main
git checkout integration && git merge main
```

Resolve conflicts on `integration` (fixes needed to adapt fork code to new
upstream APIs are committed there too). `git rerere` is enabled, so a resolved
conflict is replayed automatically next time. Before a risky update, create a
backup branch such as `backup/integration-before-upstream-<date>`.

If a feature branch is in progress during an update, merge `integration` into
it (or rebase it onto `integration`) afterwards.

## Upstream-friendly changes

Upstream changes heavily touched files often. To keep updates cheap:

- Prefer new modules or crates over large edits to existing upstream files.
- `Cargo.toml` (workspace `members` and `[workspace.dependencies]`): add one
  entry per line, in alphabetical order. Don't reformat surrounding entries.
- `Cargo.lock`: never resolve conflicts by hand. Take either side, then run
  `cargo check -p zed` to regenerate it.
- `crates/zed/src/main.rs`, `crates/zed/src/zed.rs`: only add `init` or
  registration calls. Don't restructure.
- `assets/settings/default.json`, `crates/settings_content/**`: add new keys
  next to related keys. Don't move existing ones.
- `assets/keymaps/*.json`: add bindings inside an existing context block, or in
  a new block. Don't reformat the file.

## Changing this file

`FORK.md` lives only on `integration`, so `main` stays fast-forwardable. Edit it
on `integration` directly. If you're on a feature branch and think it needs a
change, tell the user instead.

## Building on this machine

The single worktree keeps one `target/` directory, so switching between
`integration` and a feature branch only rebuilds what changed. Don't delete
`target/` and don't run `cargo clean`. Prefer `cargo check -p <crate>` and
`cargo test -p <crate>` over building the whole workspace.
