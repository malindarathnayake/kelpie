# Changelog

## Unreleased

### Fixed
- The Windows installer now installs every agent in `-Integrations claude,codex,cursor`; `powershell -File` had passed the list as one name, so no hooks were installed.

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
