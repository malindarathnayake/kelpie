# Security

Kelpie is open source and maintained by a security engineer. Releases are built only by GitHub Actions from a tagged commit, with pinned dependencies, secret, dependency and workflow scanning, signed build provenance, a published SBOM and published checksums. The binaries are not yet code-signed.

As with any binary from the internet, verify before you run it: check the checksum and the provenance attestation below, read the SBOM, or build from source.

## Reporting a vulnerability

**Do not open a public issue for security reports.**

Report privately through GitHub: [Report a vulnerability](https://github.com/malindarathnayake/kelpie/security/advisories/new). Include the affected version (`kelpie --version`), your operating system, steps to reproduce and the impact you see.

Kelpie has one maintainer. These are best-effort targets, not a guarantee:

- **Acknowledgment:** within 72 hours.
- **Triage:** within 7 days.
- **Fix:** critical and high issues target a patched release within 14 days of triage; medium and low issues go into the next release.
- **Credit:** reporters are credited in the advisory, with their permission.

Only the latest release receives security fixes.

If the issue is in code Kelpie inherited from [herdr](https://github.com/herdrdev/herdr) and also affects herdr, say so; I will coordinate with herdr's maintainers before anything is published.

## Verifying a download

Every release includes `SHA256SUMS.txt`, the same hashes in the release notes, a CycloneDX and SPDX SBOM, and a Sigstore-signed build-provenance attestation for each package.

```bash
# Checksum (Linux; on macOS use `shasum -a 256 -c`)
sha256sum -c SHA256SUMS.txt --ignore-missing

# Provenance: proves the file was built by this repository's release workflow
gh attestation verify kelpie-macos-arm64.tar.gz --repo malindarathnayake/kelpie
```

```powershell
# Windows: compare with the line in SHA256SUMS.txt
Get-FileHash .\kelpie-windows-x64.zip -Algorithm SHA256
gh attestation verify .\kelpie-windows-x64.zip --repo malindarathnayake/kelpie
```

Latest files:

- [`SHA256SUMS.txt`](https://github.com/malindarathnayake/kelpie/releases/latest/download/SHA256SUMS.txt)
- [SBOM, CycloneDX](https://github.com/malindarathnayake/kelpie/releases/latest/download/kelpie-sbom.cdx.json)
- [SBOM, SPDX](https://github.com/malindarathnayake/kelpie/releases/latest/download/kelpie-sbom.spdx.json)

An attestation proves where and how a file was built. It does not prove the code is free of bugs; the checks below are what back that.

## What Kelpie does

### In the app

- **Local only.** The server listens on a local socket that only you can use: mode `0600` on Linux and macOS (`src/api/server.rs`), and on Windows a named pipe whose access list grants only SYSTEM and the owner (`src/ipc.rs`, tested by `tests/windows_server_security.rs`).
- **The supervisor is off by default.** It only acts on a pane after you delegate control from an attached Kelpie window. No API or CLI call can grant delegation.
- **In-app updates are off** until Kelpie publishes its own update feed (`RELEASE_FEED_ENABLED` in `src/update.rs`). Update by downloading a new release.

### In the build and release

| Control | Why |
| --- | --- |
| Every GitHub Action pinned to a full commit SHA, checked for impostor commits | Tags can be force-pushed to malicious code (the March 2026 `trivy-action` compromise). A SHA cannot be re-pointed. |
| Scanners downloaded as exact versions and checked against a SHA-256 | No `curl \| sh`, no `latest`, no third-party scanner actions. |
| Read-only token by default; write access only in the final publish job | Code under test never holds a write token. |
| Release split into build and publish jobs | The job that creates the release never runs project code. |
| No build cache in release builds | A poisoned cache cannot reach a published binary. |
| Strict tag check: `vX.Y.Z` equal to `Cargo.toml` | A stray or mistyped tag cannot publish. |
| `cargo build --locked` | The binary is built from exactly the dependency versions in `Cargo.lock`. |
| Dependabot with a 7–14 day cooldown | Updates arrive, but not on the day a possibly hijacked release is published. |

### Scanning

`.github/workflows/security.yml` runs on every push to `main`, every pull request and weekly.

| Scanner | Covers | Fails on |
| --- | --- | --- |
| CodeQL (`security-extended`) | Rust source and the workflow files | Code scanning alerts |
| gitleaks 8.30.1 | Secrets in the tree and the full git history, including herdr's | Any finding |
| zizmor 1.30.1 (`pedantic`) | Workflow injection, permissions, credential persistence, unpinned or impostor actions | Any finding |
| Syft 1.52.0 + Grype 0.119.0 | SBOM of every Rust dependency in `Cargo.lock`, plus the vendored terminal engine, the libraries it compiles in and the bundled Windows ConPTY (`scripts/sbom_add_vendored.py`), checked for known vulnerabilities | High or critical (also gates every release) |

Reviewed false positives are listed one by one in `.gitleaksignore`, each with the reason.

## Not yet in place

- **Code signing.** Windows SmartScreen and macOS Gatekeeper will warn until the binaries are signed and notarized. This is planned.
- **The vendored terminal engine** (`vendor/libghostty-vt`, Zig) is listed in the SBOM but not analysed by CodeQL; its upstream changes are reviewed when it is updated (`vendor/libghostty-vt.patches.md`).

## Scope

In scope: the Kelpie binary, its local API and socket, the installers, this repository's workflows and the published release files.

Out of scope: the AI agents you run inside Kelpie (Claude Code, Codex, Cursor and others; report to their vendors), problems that need an already-compromised machine or user account, and social engineering.
