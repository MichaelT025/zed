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
- The startup flags were derived from Devin 3.10.67 (protocol 1.48.2). Re-check them
  when Devin updates, since an incompatible server fails at startup.
