# Kelpie placeholder addresses

Kelpie has no website, repository host or release feed yet. Every upstream herdr
address was replaced with a placeholder on the reserved `kelpie.example` domain
(RFC 2606), which never resolves, so no link, installer or update can reach a
site Kelpie does not own.

When Kelpie has real addresses, replace these and turn the release feed back on.

| Placeholder | Meant for | Where it is used |
|---|---|---|
| `https://kelpie.example` | Website home | `Cargo.toml` (`homepage`), `src/main.rs` (`--version` output, config comments), docs |
| `https://kelpie.example/source` | Source repository (issues, releases) | `Cargo.toml`, `crates/ghostty-vt/Cargo.toml` (`repository`), docs, `distribution/*.json` asset URLs |
| `kelpie.example/kelpie` | `owner/repo` shorthand (Homebrew tap, Nix flake, badges) | docs |
| `https://kelpie.example/latest.json`, `/preview.json` | Release manifests | `src/update.rs`, `src/remote/attach.rs`, `distribution/install.*` |
| `https://kelpie.example/install.sh`, `.ps1`, `.cmd` | Installers | `distribution/`, docs |
| `https://kelpie.example/agent-detection/index.toml` | Agent-detection manifest catalog | `src/detect/manifest_update.rs` |
| `https://kelpie.example/agent-guide.md`, `/llms.txt` | Agent guide | `src/cli.rs`, docs |
| `https://kelpie.example/plugins/` | Plugin marketplace | docs |
| `https://kelpie.example/docs/...` | Documentation pages | README, docs |

## Turning updates back on

`src/update.rs` holds `RELEASE_FEED_ENABLED`, currently `false`. While it is false,
`kelpie update`, the background update check, the Homebrew version check and the
remote-attach binary download all refuse to fetch and report
"updates are disabled until Kelpie has its own release feed". Set it to `true`
only after the manifest URLs point at a feed Kelpie publishes.

## Kept on purpose

- `ogulcancelik/herdr-plugin-examples`: a real third-party repository; the
  examples target herdr's plugin API and may need porting.
- `github.com/herdrdev/herdr` in the README credits.
- `SPONSORS.md`: herdr's sponsors, unchanged.
- `docs/Brand/`: the brand guide, which says "built on herdr" deliberately.
