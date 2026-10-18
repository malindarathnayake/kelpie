# Changelog

## Unreleased

## [0.1.2] - 2026-10-18

### Fixed
- The Windows installer now installs every agent in `-Integrations claude,codex,cursor`; `powershell -File` had passed the list as one name, so no hooks were installed.
- The server no longer crashes when a cursor move carries a link onto a full page. (herdr #5303)
- Claude Code questions show as blocked in named sessions. (herdr #5192)
- Codex sessions no longer start with a "clamping Interrupt hook timeout" warning. Kelpie no longer installs Codex turn hooks; Codex status comes from screen detection, and prompt receipts still work. Run `kelpie integration install codex` to update. (herdr #5278)
- On Windows, agent hooks work when the user folder name contains non-ASCII characters, such as Korean. Reinstall your integrations to update. (herdr #5270)
- On Windows, plugin panes start even when a folder on `PATH` has the same name as the plugin's program. (herdr #5269)
- Prefix shortcuts work when the terminal sends the next key as plain text, as macOS input methods and Ghostty `text:` keybinds do. Workspace navigation had the same problem. (herdr #5277)
- Codex panes show as blocked while Codex asks whether to update at startup, instead of idle. (herdr #5174)
- Clicking `! auth`, `! error` or a reconnecting status on a machine retries the connection at once; a notice shows the error only if the retry fails. (herdr #5232)
- A remote server that accepts the connection but never answers the handshake no longer leaves the client waiting forever. (herdr #5242)
- A remote server that is still starting or busy gets up to a minute to send its first snapshot before the connection is marked failed. (herdr #5241)
- A connection attempt that is replaced or abandoned is now cancelled and its SSH bridge cleaned up. (herdr #5247)
- After the computer wakes from sleep, machines reconnect right away instead of waiting out the retry delay. (herdr #5249)
- When a shared SSH connection can no longer open channels but a fresh one works, Kelpie drops the stale connection and reconnects. (herdr #5252)
- Adding a Windows machine or using `--remote` with one no longer prints raw PowerShell progress records. (herdr #5282)
- Windows machines whose SSH shell is not a POSIX shell are detected correctly during setup. (herdr #5275)
- Machines reached through OpenSSH for Windows with a WSL bash login shell no longer fail with `remote binary discovery failed`. (herdr #5308)
- On macOS, quitting the terminal app that started Kelpie no longer stops the server and every pane. (herdr #5236)
- Dragging a window no longer ends the session when the terminal briefly reports a zero-sized grid. (herdr #5310)
- `kelpie agent digest` accepts `--page-cursor=VALUE` and `--trace-id=VALUE`, and `kelpie agent undelegate` accepts `--pane=ID`, like the other agent commands.
- The delegation border and badge clear as soon as `kelpie agent undelegate` revokes a pane.

### Changed
- The API schema now gives the exact format of the supervisor `submit_id` and resume `session_id`, and says that submitted `text` is limited to 8192 bytes, not characters.

### Security
- Revoking a pane delegation is saved at once. Before, a permanent delegation revoked with no other change could come back after a crash or restart.
- The SBOMs now also list the vendored terminal engine (libghostty-vt), the libraries it compiles in (uucode, simdutf, highway, wuffs) and the bundled Windows ConPTY.

## [0.1.1] - 2026-10-16

### Security
- Releases now include a CycloneDX and SPDX SBOM, a SHA-256 table in the release notes, and a signed build-provenance attestation for each package. See SECURITY.md for how to verify a download.
- Every push, pull request and weekly run is scanned with CodeQL, gitleaks, zizmor and Grype; a high or critical dependency advisory blocks the release.
- Updated `rand` to 0.8.6 (GHSA-cq8v-f236-94qc, low severity).

## [0.1.0] - 2026-10-16

First Kelpie release. Kelpie is a fork of [herdr](https://github.com/herdrdev/herdr), based on herdr 0.9.3. herdr's own history up to that point is in [herdr's changelog](https://github.com/herdrdev/herdr/blob/master/CHANGELOG.md).

### Added
- Kelpie name, icon, config, state and socket, so it runs side by side with an installed herdr.
- A supervisor that watches agent panes and can act on them when you delegate control. Off by default.
- A ctrl+b hint bar that lists the most useful prefix keys.
- Windows x64 and macOS Apple Silicon builds on GitHub Releases.
