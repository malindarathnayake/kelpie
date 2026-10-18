# Changelog

## Unreleased

### Fixed
- The Windows installer now installs every agent in `-Integrations claude,codex,cursor`; `powershell -File` had passed the list as one name, so no hooks were installed.
- The server no longer crashes when a cursor move carries a link onto a full page. (herdr #5303)
- Claude Code questions show as blocked in named sessions. (herdr #5192)
- Codex sessions no longer start with a "clamping Interrupt hook timeout" warning. Kelpie no longer installs Codex turn hooks; Codex status comes from screen detection, and prompt receipts still work. Run `kelpie integration install codex` to update. (herdr #5278)
- On Windows, agent hooks work when the user folder name contains non-ASCII characters, such as Korean. Reinstall your integrations to update. (herdr #5270)
- On Windows, plugin panes start even when a folder on `PATH` has the same name as the plugin's program. (herdr #5269)
- Prefix shortcuts work when the terminal sends the next key as plain text, as macOS input methods and Ghostty `text:` keybinds do. Workspace navigation had the same problem. (herdr #5277)

### Security
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
