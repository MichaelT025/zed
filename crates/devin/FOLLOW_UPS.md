# Devin provider follow-ups

Work deliberately deferred from the initial implementation.

## Platform support and server location

The language server is only looked up at Devin's default Windows install location,
`%LOCALAPPDATA%\Programs\Devin\resources\app\extensions\windsurf\bin\language_server_windows_x64.exe`
(see `default_binary_path` in `src/language_server.rs`). Non-Windows builds fail with
"Devin edit predictions are currently only supported on Windows".

To do:

- Locate Devin on macOS and Linux. The binary name follows the
  `language_server_<os>_<arch>` pattern; the app root and `product.json` location need
  to be confirmed against real installs.
- Add a setting for the language server path (or the Devin app root), for non-default
  installs and for when Devin's layout changes between versions.
- The startup flags were derived from Devin 3.12.59 (protocol 1.48.2). Re-check them
  when Devin updates, since an incompatible server fails at startup.

## Tab suggestions stream

The tab path now uses `StreamTabSuggestions`, a Connect server-streaming RPC, as of
Devin 3.12.59. The route, request envelope, and response framing were derived from that
build; `HandleStreamingTabV2` no longer exists there. Re-check the route and frame fields
when Devin updates, since a missing route now reports itself as such.

To do:

- Apply `tabJumpCandidate` frames. Devin's client converts the candidate into a jump
  locally with its `computeTabJump` algorithm (line matching against `completionText`,
  including the `[---TABJUMP_CONTENT_DELIMITER---]` marker, the `<|cursor|>` sentinel, and
  the `tab-jump-replacement-boundary-ratio` feature flag). The frames are only logged
  today, so a jump the server reports solely this way is dropped. `response.tabJump` is
  still consumed.
- Validate suggestions against `triggerState.contents`: it echoes the document text the
  suggestion was computed for, and Devin's client drops a suggestion when it no longer
  matches the buffer. Zed currently relies on the diff's `oldStr` check inside
  `convert_diff`, which covers edits but not jump targets.
- Consider `triggerState.finalContents` as an alternative edit source. Devin's client
  recomputes the edit from `contents` and `finalContents` and ignores `characterDiff`.
- The completion timeout is 15s, while Devin's own client aborts a tab request after 5s
  and relies on prefetched suggestions. The stream is closed after the first usable frame,
  so no prefetch queue exists.
