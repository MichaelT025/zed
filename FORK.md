# Fork coordination (read before working)

This repository is a personal fork of Zed (`origin` = `MichaelT025/zed`,
`upstream` = `zed-industries/zed`). Two features are being developed **in
parallel, in two separate git worktrees, usually by two different agents**.
You are working on one of them. The other one exists, is changing at the same
time, and will eventually be merged together with yours.

## Branches

| Branch                      | Purpose                                                    |
|-----------------------------|------------------------------------------------------------|
| `main`                      | Mirror of `upstream/main` plus this file. No feature work. |
| `feature/vscode-git-panel`  | VS Code–style Source Control panel.                        |
| `feature/devin-completions` | Devin as an edit prediction (inline completion) provider.  |
| `integration`               | Merge-only branch that combines both features.             |

Find out which feature you own with `git branch --show-current`. Run
`git worktree list` to see where the other worktree lives on disk.

## Ownership

Each feature owns a set of files. Stay inside your own area. If you need a
change outside it, keep that change as small as you can (see "Shared hot
spots" below).

**`feature/vscode-git-panel`** owns:
- `crates/git_ui/**` (especially `git_panel.rs`, `git_panel_settings.rs`, `project_diff.rs`)
- `crates/git_ui_core/**`
- `crates/git/**` and `crates/project/src/git_store*` when the panel needs new repository data

**`feature/devin-completions`** owns:
- a new crate, `crates/devin/` (or a similar name), that contains the provider
- `crates/edit_prediction/**`, `crates/edit_prediction_types/**`, `crates/edit_prediction_ui/**`
- `crates/zed/src/zed/edit_prediction_registry.rs`
- `crates/settings_ui/src/pages/edit_prediction_provider_setup.rs`

Do not modify files that the other feature owns. You may read them freely: in
your own worktree, or on the other branch with
`git show feature/<other>:path/to/file`.

## Shared hot spots

Both features are likely to touch the files below. Merge conflicts happen
here, so change them with care:

- `Cargo.toml` (workspace `members` and `[workspace.dependencies]`): add one
  entry per line, in alphabetical order. Don't reformat or reorder the
  surrounding entries.
- `Cargo.lock`: never resolve conflicts here by hand. On `integration`, take
  either side and then run `cargo check -p zed` to regenerate it.
- `crates/zed/Cargo.toml`, `crates/zed/src/main.rs`, `crates/zed/src/zed.rs`:
  only add new `init` or registration calls. Don't restructure.
- `assets/settings/default.json`, `crates/settings_content/**`: add new keys
  next to related keys. Don't move existing ones.
- `assets/keymaps/*.json`: add bindings inside an existing context block, or
  in a new block. Don't reformat the file.
- `.rules`, `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `FORK.md`: don't edit these
  on a feature branch (see "Changing this file").

Before you edit a hot spot, check what the other side has already done there:

```sh
git diff main...feature/<other> -- <path>
git log --oneline main..feature/<other>
```

## How integration works

1. All feature work is committed on its own feature branch. **Never commit
   feature code directly to `integration`.**
2. Keep each feature branch rebased on `main`:
   `git fetch upstream && git rebase main`, after `main` has been updated.
3. To combine the features, someone (usually the user) runs:
   ```sh
   git checkout integration
   git merge main feature/vscode-git-panel feature/devin-completions
   ```
   `git rerere` is enabled, so a conflict that has been resolved once is
   resolved the same way automatically next time.
4. If a conflict comes from a real design overlap rather than ordering, fix it
   on the relevant feature branch. Don't fix it only on `integration`; the fix
   would be lost the next time `integration` is rebuilt.
5. `integration` can be thrown away and rebuilt from `main` plus both feature
   branches at any time. Never rely on commits that exist only there.
6. A feature is "integration-ready" when `cargo check -p zed` and its own
   crate tests pass on the feature branch by themselves. Each feature must
   build and work without the other.

## Updating from upstream

Only `main` pulls from upstream:

```sh
git checkout main && git fetch upstream && git merge --ff-only upstream/main
```

Then rebase each feature branch onto `main`.

## Changing this file

`FORK.md` is shared by both worktrees. Edit it **only on `main`**, then rebase
both feature branches so that each side sees the same rules. If you're on a
feature branch and think it needs a change, tell the user instead of editing
it.

## Building on this machine

Each worktree has its own `target/` directory, so the first build in a new
worktree is a full Zed build. Don't delete `target/` and don't run
`cargo clean`. Prefer `cargo check -p <crate>` and `cargo test -p <crate>`
over building the whole workspace.
