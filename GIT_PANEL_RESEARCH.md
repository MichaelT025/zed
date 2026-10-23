# VS Code–style Source Control panel: background research

Context for the agent working on `feature/vscode-git-panel`. This file
summarizes research done on 2026-10-22/23 into what upstream Zed has, what
others tried, and why it hasn't landed. Read `FORK.md` first for branch and
ownership rules; this file only covers the feature itself.

## Goal

The user dislikes Zed's current Git panel and wants VS Code's Source Control
layout: **Changes** at the top and a **commit graph** at the bottom, visible at
the same time in the same sidebar panel, with collapsible and resizable
sections. This is a personal fork, so upstream acceptance isn't required, but
keeping the change upstreamable is a nice-to-have (see "If we ever upstream
this").

## What upstream Zed already has

- **Git Graph** (`crates/git_ui`, `git_graph`): shipped through 2026. It opens
  as a **center-pane tab**, not in the dock. It has lane rendering, lazy
  loading, search (including SHA), resizable/hideable columns, a commit
  details panel with the message rendered as Markdown, split diff, ref labels
  drawn in the graph, and a context menu (copy/create tag, custom git
  commands). It replaced the old file-history view.
- **Git panel**: has separate **Changes** and **History** tabs. Only one shows
  at a time, which is the thing the user wants changed.

Reuse the Git Graph's lane layout and painting rather than writing a new one.

## Prior attempts (both closed upstream; read their diffs)

| PR | What it did | Outcome |
|---|---|---|
| [#64871](https://github.com/zed-industries/zed/pull/64871) by TevfikBGunes | Collapsible, vertically resizable compact graph under the changes list, ref picker, "go to current" button, hover tooltips, button to expand into the full Git Graph, and cancelable fetch/pull/push | Closed 2026-09-29: no linked issue/discussion, UX diverged from the existing Git Graph |
| [#65097](https://github.com/zed-industries/zed/pull/65097) by mansoorkhan96 | Opt-in `"git_panel": { "compact_graph": true }`. Collapsible Graph section below Changes (replaces the last-commit row), click a commit to expand its files inline, click a file to open that file's diff via `CommitView`, hover popover over the editor. Moved the Git Graph's lane layout/painting into shared helpers with no behavior change. Also fixed a `CommitView` bug where deleted files showed an empty view | Closed 2026-10-02 |

Either PR is a good starting point. You can fetch one with
`git fetch upstream pull/65097/head:pr-65097` (or `64871`). #65097 is the
smaller one and stays closer to existing panel code. Expect it to need rebasing
onto current `main`.

## Why maintainers rejected it (design constraints worth respecting)

From the review on #65097:

1. **Low demand.** They found no issues or discussions asking for a graph in
   the Changes tab.
2. **Space.** The panel is narrow at default width, so file paths and commit
   subjects got truncated. "A feature has to work with the defaults", and
   making it opt-in doesn't remove the maintenance cost.
3. **The shape they'd accept:** improve the **History tab** instead of adding
   a new section, after a GitHub Discussion to agree on the design.

Takeaways for this fork:
- Make it look good at the **default panel width**. Truncation was the
  concrete complaint.
- Build it as an evolution of the **existing History tab** (e.g. an accordion
  layout that stacks Changes and History, with graph lanes in History) rather
  than a third parallel commit list. That keeps one code path and is the
  version upstream said they might accept.
- Put it behind a setting so it can be switched off.

## Related upstream items

- Discussion [#65654](https://github.com/zed-industries/zed/discussions/65654):
  "Git Panel: Option to show Changes and History as accordion sections in the
  same view (like VS Code)". This is exactly this feature. It proposes
  `"git_panel": { "layout": "accordion" | "tabs" }` with a draggable split and
  remembered collapsed state. Open, very few upvotes.
- Discussion [#26866](https://github.com/zed-industries/zed/discussions/26866)
  "Add Git Graph" (312 upvotes, closed: shipped as the tab).
- Discussion [#52191](https://github.com/zed-industries/zed/discussions/52191)
  and PR [#61111](https://github.com/zed-industries/zed/pull/61111): a
  multi-repository Git panel. This is likely to touch the same panel code, so
  watch it for conflicts.
- Open Git Graph PRs that may land and conflict: branch filter #63851,
  keyboard navigation #65927, whole-row selection painting #65267, panel
  button toggles the Graph tab #62993, preserve selection across refresh
  #63322, extract `canvas`/`graph` out of `git_graph.rs` #57019 (that
  refactor would help with sharing lane painting).
- Known Git Graph bugs worth not inheriting: no auto-refresh #53594, high GPU
  usage #54040, ref label gutter truncation #65681, stale "Loading…" details
  #59165.

## Why this has to be a fork, not an extension

Zed extensions can't render UI: no panels, no custom views. The RFC for a
visual extension API ([#53403](https://github.com/zed-industries/zed/discussions/53403))
has a maintainer response saying it's not likely in the near future. No
third-party plugin exists for this.

## If we ever upstream this

Zed's contributing guide requires a linked issue or Discussion before feature
PRs. The path would be: comment on #65654 with a design (mockups at default
width, how it extends the History tab), then send small focused PRs. The
`CommitView` deleted-file fix from #65097 is a good standalone first PR if it's
still broken on `main`.
