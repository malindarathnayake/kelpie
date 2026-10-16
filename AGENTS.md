# kelpie

Terminal based agent runtime for coding agents.

## Scope and Audience

These instructions are layered.

- Unless a section explicitly says it is maintainer-only, local-machine-only, or
  external-contributor-only, treat it as universal project guidance.
- Universal project rules apply to every agent working on Kelpie, including forks.
- Maintainer accounts are listed in `.github/MAINTAINERS`. Treat the acting
  account as a verified maintainer only when its username is listed there, the
  configured remote is the canonical `malindarathnayake/kelpie` repository, and the
  authenticated account has write access to that repository. If any condition
  cannot be verified, skip maintainer workflow and follow the external
  contributor rules instead.
- External contributor rules apply whenever the acting GitHub account is
  not a verified maintainer, the work is happening in a fork, or the account
  cannot be determined.

## Universal Project Rules

### Principles

- **State is separated from runtime.** `AppState` is pure data, testable without PTYs or async. `PaneState` is separate from `PaneRuntime`. Workspace logic doesn't need real terminals.
- **Render is pure.** `compute_view()` handles geometry and mutations. `render()` takes `&AppState` and only draws. Never mutate state during render.
- **No god objects.** If a module is doing too many things, split it. `app/` is already split into state, actions, and input. Keep it that way.
- **Platform code is isolated.** OS-specific behavior lives in the matching `src/platform/<os>.rs` file, with only shared traits, types, wrappers, and testable contracts in `src/platform/mod.rs`. Core modules don't have `#[cfg(target_os)]`.
- **Detection is decoupled.** The detector reads a screen snapshot, never touches the parser or viewport state.
- **Screen detection is evidence-based.** When changing `src/detect/manifests/`, first capture the relevant bottom-buffer state with `kelpie agent read <pane> --source detection --format text` and, when styling or alternate screen behavior matters, `--format ansi`. Decide which visible controls are invariant, which are alternatives, and encode them as explicit AND/OR gates. Do not match whole-pane incidental text, and do not use the user-visible viewport for agent status because users can scroll it.
- **UI patterns should be reused.** Kelpie is a mouse-first TUI. New dialogs, onboarding, settings, and post-update flows should follow the existing UI/UX language and interaction patterns instead of inventing one-off screens. Prefer reusing existing modal/screen structure, affordances, and close actions so the app feels consistent.

### Multiplicative performance paths

Treat work reachable from view computation, rendering, background-pane resizing,
PTY parsing, detection, and client frame fanout as multiplicative. Before adding
work, identify its frequency and cardinality: per byte, event, or render × panes,
tabs, or workspaces × attached clients.

Inside pane-scaled render and layout loops:

- Use narrow terminal-state accessors. Do not collect aggregate input state,
  format terminal snapshots, inspect process trees, perform filesystem I/O, or
  allocate when one scalar fact is enough.
- Keep terminal-core lock duration minimal.
- Preserve hidden-source and retained-render early exits. Hidden panes still
  parse output, but their output must not trigger presentation work merely to
  keep terminal or detection state current.
- When a change adds or widens work in one of these loops, profile fixed geometry
  with 1 and at least 15 populated panes and report the scaling delta. Use
  `just bench-render-scale` to exercise both background-workspace and active-pane
  cardinality when applicable.

Prefer deterministic operation or architecture tests to wall-clock CI limits.
Performance benchmarks are supporting evidence, not substitutes for behavioral
coverage. Before a stable release, `just bench-release-smoke` must compare the
candidate with the current stable binary under hidden and visible output. When
the result moves materially or when validating performance work, repeat it with
`KELPIE_PERF_SAMPLE_SECONDS=60` and investigate the affected scenario.

### Runtime/client boundary guardrail

Kelpie is migrating toward a server-owned runtime protocol with the TUI as one client. New work should not deepen the current server/TUI coupling.

Before adding state, API fields, events, commands, or socket messages, classify the feature:

- Shared runtime/session fact: belongs in server state and should be exposed through the JSON API/event path when practical.
- TUI presentation state: belongs only in the TUI/client layer.

Do not add new shared behavior that only works through the private TUI client socket. Use neutral server/API names, not UI-surface names like sidebar, row, card, or widget.

Examples:

- Pane/agent metadata, process state, terminal state, events: server/runtime.
- Sidebar layout, token placement, colors, selection, modals, mouse/viewport state: TUI/client.
- Workspace/tab/pane remain shared session organization for now, but avoid making them mandatory identity for unrelated runtime features.

### Stable client endpoint contract

The client-owned TUI endpoint generation is independent from the private same-install protocol. Generation 1 is the compatibility floor for Local, SSH, and Cloud connections and must remain available unless retired for a security reason.

- Named core codecs are immutable. Do not add, remove, reorder, or reinterpret fields or enum variants reachable from a published codec. Introduce a new codec name and keep the old codec as a fallback instead.
- Keep baseline JSON handshake and snapshot fields required. New JSON fields must be optional or have field-specific defaults; new enum values need an `Unknown` fallback where older clients can safely ignore them.
- Add server features through advertised API methods and optional snapshot data when possible. A missing optional feature must disable only that action, not reject the connection.
- Do not change the meaning or load-bearing parameter shape of an advertised endpoint method. If an old server could ignore a new field and incorrectly report success, add a new method name or a separately advertised capability and omit that field without it.
- Missing methods, rejections, timeouts, and unavailable servers are client-local outcomes. They must not disconnect other compatible servers, and typing in a pane must not dismiss their notices.
- Frozen endpoint fixtures, bincode digests, wire-tag tests, and `tests/fixtures/endpoint-method-shapes-v1.json` are compatibility contracts. Never update a generation-1 expectation merely to bless a wire change; create and negotiate a new codec or method.
- Stable and preview update manifests advertise `endpoint_generation`. Keep release tooling aligned so an older updater knows when a new server generation really requires replacement.
- Existing-value digests cannot detect an appended enum variant. Review every enum reachable from a frozen codec as append-closed even when tests remain green.

## Maintainer Workflow

This section applies only to verified maintainers as defined under Scope and
Audience. Everyone else must skip this section and follow the external
contributor rules.

### Multi-agent isolation

Read-only investigation can happen in the shared checkout.

Small changes or small tasks are fine in the default main worktree. If you find unrelated implementation changes already in progress in the main worktree, use a dedicated worktree instead. Use a dedicated worktree for bigger features too.

Use this layout:

- shared integration checkout: `../kelpie`
- task worktrees: `../kelpie-worktrees/<task-slug>`
- task branches: `issue/<id>-<slug>` when an issue exists

Do all code edits, tests, and validation inside the task worktree.

Commit on the task branch in that worktree.

For substantive feature and bug-fix work, default to opening a pull request instead of pushing `main` directly. Small, low-risk changes and documentation-only updates can use a lighter workflow when the maintainer prefers it.

Immediately before opening a pull request, fetch `origin` and make sure the task branch is based on the current `origin/main`; rebase it when behind, then rerun relevant validation before pushing. If `main` advances while the pull request is under review and GitHub marks it behind, update the branch and repeat checks and bot review on the new head.

After opening or updating a pull request, monitor all checks to completion with `gh pr checks --watch` or an equivalent command. Treat Greptile and CodeRabbit as part of CI: wait for both to review the latest pushed commit, not only for the build and test jobs to pass. Evaluate every actionable finding. Fix findings you agree with and reply with the fix; reply inline with a concise technical reason when you disagree. After any fix, wait for CI and both review bots again on the new head.

When the current pull request head is green and both bot reviews are complete, report that it is ready and stop. Never merge a pull request; the maintainer performs the final merge.

If the current session is already inside an isolated task worktree, keep using it. Do not create nested worktrees.

Before committing, propose the commit message and get alignment.

After the maintainer confirms the change is integrated, update the shared checkout, remove the task worktree, and delete the task branch locally and remotely.

## Testing

Use `just` recipes by default instead of invoking cargo or scripts directly.

```bash
just test               # cargo nextest + maintenance script tests
just check              # formatting check + cargo nextest + maintenance script tests
```

Run `just check` before committing unless the maintainer explicitly accepts narrower validation. Do not bypass failing checks; fix the failure or explain exactly why a narrower check is enough.

Windows MSVC cross-compilation from Unix requires SDK/CRT headers and libraries.
Install `xwin` with `cargo install xwin --locked`, then run
`just setup-windows-cross` once and accept Microsoft's SDK license when prompted.
This downloads the SDK directly from Microsoft; no Windows machine is required.
The SDK and Zig libc configuration live at `~/.local/share/kelpie/windows-cross/`,
shared by worktrees. `just windows-lint` and the Windows stage of `just check`
use this configuration automatically. To use another SDK, set
`LIBGHOSTTY_VT_WINDOWS_LIBC` to its Zig libc configuration file.
Setup accepts `--accept-license` for explicit noninteractive license acceptance;
normal checks never download the SDK. Native Windows builds auto-detect their
installed SDK. Native Linux/macOS builds do not need the Windows SDK.

Unit tests live next to the code (`#[cfg(test)] mod tests`). New `AppState` or `Workspace` behavior should be testable with `AppState::test_new()` and `Workspace::test_new()` without PTYs.

For broad refactors or release-risk regressions, classify the risk before editing. Treat changes as refactor-risk when they touch two or more core surfaces, persisted state, protocol/API IDs, workspace/tab/pane identity, restore/handoff, agent detection authority, or UI/input state projection. Before moving code, identify the protected behavior and add or name characterization tests. Identity/state refactors should use the test-only invariants `AppState::assert_invariants_for_test()` or `Workspace::assert_invariants_for_test()` with adversarial state from `AppState::test_with_adversarial_identity_state()` or `Workspace::test_adversarial_identity_state()`. Run a roundtable for broad refactors and release-risk regressions, not for routine local fixes.

When testing a new Kelpie build from inside an existing Kelpie session, use
`cargo run -- ...` and clear inherited Kelpie socket overrides so the debug
binary talks to the debug `kelpie-dev` server instead of the installed stable
server:

```bash
env -u KELPIE_SOCKET_PATH -u KELPIE_CLIENT_SOCKET_PATH cargo run -- <command>
```

## Agent Detection Updates

Agent detection changes should use the manifest hot-reload loop. Use the project-local `kelpie-throwaway-repro` skill to create a disposable named session and drive the real agent UI through Kelpie's CLI/API into the target state. Read the pane with `kelpie agent read <pane> --source detection --format text` and inspect matching with `kelpie agent explain <pane> --json`. Update the bundled manifest in `src/detect/manifests/<agent>.toml`, copy that manifest to the local override path at `~/.config/kelpie/agent-detection/<agent>.toml`, then run `kelpie server reload-agent-manifests` against the session under test. Before writing the override, check whether one already exists; never overwrite or remove a pre-existing override without alignment. Once the rule is correct, remove the temporary override or restore the previous one exactly so the committed bundled manifest remains the source of truth.

Unit-test Kelpie's detection engine, not individual CLI agents' screen or title conventions. Use synthetic manifests and minimal input strings to test parsing, regions, matching, AND/OR/NOT gates, rule priority, skip-state semantics, source precedence, cache reload behavior, and update flow. Keep bundled-manifest schema validation, process identification, and integration hook/protocol tests. Do not add tests that classify captured or invented CLI screens against bundled agent rules, or freeze an agent's specific detection rule IDs and priorities.

Validate agent-specific detection behavior with live smoke tests through the manifest hot-reload loop above. Exercise the changed state and nearby transitions (idle, working, blocked, and background work where supported), including relevant optional OSC settings. Record the CLI version, observed signals, and outcomes. Passing engine tests proves the rules execute as written; it does not prove compatibility with the current CLI.

`distribution/agent-detection/` is the remotely published catalog for released clients. Keep changes for already released agents aligned with their bundled manifests unless the validator records an exact compatibility exception. A newly bundled agent that current stable clients cannot identify may remain unpublished behind an exact version-and-digest exception, but it must be added to the catalog and the exception removed before the first stable release that ships it. `just release-docs-check` enforces that no unpublished exceptions remain.

## Vendored libghostty-vt

`vendor/libghostty-vt.vendor.json` records the upstream source commit currently vendored.

Local patches on top of the vendored source must be tracked in `vendor/libghostty-vt.patches.md` and stored as patch files under `vendor/patches/libghostty-vt/`. Each entry should say why the patch exists, the Kelpie issue, upstream PR/discussion, vendored base commit, touched files, verification, and the exact removal condition.

When updating libghostty-vt, check every active patch in `vendor/libghostty-vt.patches.md`. If the new upstream commit contains the fix, remove the local patch and index entry, then rerun the listed verification. If not, reapply the patch on top of the new vendored source.

`just check` runs maintenance tests that verify local libghostty-vt patch files are listed in the index and reverse-apply cleanly against the vendored tree. Do not leave a patch file untracked or an indexed patch unapplied.

## Docs

`skills/kelpie/SKILL.md` tracks the latest Kelpie release because the unversioned `npx skills add malindarathnayake/kelpie --skill kelpie -g` command installs it from `main`. Do not update this file in feature work. Review and update it when preparing a release, and include the change in the release commit with the `Cargo.toml` version bump.

Unreleased docs live in `docs/next/website/src/content/docs/`. Update those when a user-facing change needs docs before the next release. They are committed drafts but are never production website input. `docs/next/README.md` stages root README changes. `docs/next/CHANGELOG.md` is curated when preparing a release, not maintained by normal feature and fix work.

Normal feature and fix work must not edit `docs/next/CHANGELOG.md`; this keeps long-lived branches from conflicting over one shared release file. When refreshing an older pull request, remove its changelog-only diff. Keep user-facing commit subjects descriptive and include required `refs #<issue-number>` lines so release preparation can inventory the full range. When preparing a release, use that inventory to human-write and curate the user-facing entries in `docs/next/CHANGELOG.md`; generated commit lists are source material, not final release prose. Do not add changelog entries for website-only, documentation-only, CI, build-pipeline, or repository-maintenance changes.

Normal feature/fix work should not edit root `README.md` or root `CHANGELOG.md` unless it is a focused correction or explicitly requested.

Put local PRDs, planning notes, and exploratory specs under `.local/prd/`; `.local/` is ignored and locally controlled.

## Commit Style

Use lowercase conventional commits, no emojis, and no AI co-author lines. Commit subjects feed generated release notes, so keep them descriptive.

Before committing, propose the commit message and get alignment.

When a normal feature or fix commit relates to a GitHub issue, add a commit body line `refs #<issue-number>` after the subject:

```text
fix: handle pane focus

refs #82
```

Do not use GitHub closing keywords like `fixes #<issue-number>`, `closes #<issue-number>`, or `resolves #<issue-number>` in normal commits. `main` contains unreleased work; close referenced issues when the release that contains the fix is published.

## Code Conventions

- Rust: no `unwrap()` in production code. Use `tracing` for logging. Use `#[allow]` only with a comment explaining why.
- Rust platform-specific code must be compile-gated. Put OS APIs and substantial OS behavior in `src/platform/`; when platform checks are needed elsewhere, use `#[cfg(windows)]`, `#[cfg(unix)]`, or target-specific `#[cfg(...)]` on imports, fields, functions, impls, and match arms so Windows-only code does not compile into Unix builds and Unix-only code does not compile into Windows builds. Use `cfg!(...)` only for pure cross-platform policy constants whose branches both compile on every target.
- Don't add dependencies without a reason. Check whether existing dependencies cover the need first.
- Integration asset versions (`KELPIE_INTEGRATION_VERSION` markers and matching `*_INTEGRATION_VERSION` constants) are migration versions relative to the latest released tag, not per-commit counters on `main`. If an integration asset changes multiple times between releases, bump it once from the version in the latest release.
- When changing the server/client wire protocol, compare `src/protocol/wire.rs::PROTOCOL_VERSION` against the protocol in the latest published release. Bump it when the current source protocol has already been published and the wire format changes incompatibly. Do not bump it again for multiple incompatible changes before that protocol is published. Update hardcoded protocol expectations and manual protocol fixtures in tests.

## Releases

This section is maintainer-only. If the acting GitHub account is not a
maintainer, do not run release commands, push tags, or publish assets.

Kelpie has one branch, `main`, and one release line. Each release is a `v<version>` tag on `main`:

1. Update `CHANGELOG.md` (and `docs/next/CHANGELOG.md`) with the release notes, bump `version` in `Cargo.toml`, run `cargo update -p kelpie --offline`, and commit as `release: v<version>`.
2. Run `just check`, then `just release <version>`. It checks that the tag matches `Cargo.toml` and pushes the annotated tag.
3. `.github/workflows/release.yml` builds Windows x64 and macOS Apple Silicon, then publishes a GitHub Release with `SHA256SUMS.txt`. It is marked latest, so `releases/latest/download/...` links follow it.

The release must publish these assets:

- `kelpie-windows-x64.zip`: `kelpie.exe`, its app-local ConPTY runtime, `THIRD-PARTY-NOTICES/`, `install.ps1` and `README.txt`. Never publish a bare executable for Windows.
- `kelpie-macos-arm64.tar.gz`: `kelpie`, `install.sh` and `README.txt`.

In-app updates stay off (`RELEASE_FEED_ENABLED` in `src/update.rs`) until Kelpie publishes its own update manifest; see `distribution/PLACEHOLDERS.md`.

## External contributors

Anyone may open a pull request; see `CONTRIBUTING.md`. The maintainer reviews every one and merges what is good.

When helping someone who is not a maintainer:

- Keep each pull request to one problem, with tests that fail without the fix, and run `just ci` before opening it.
- Open a Discussion first for features or changes to behavior, UI or product direction.
- Make sure the human understands and can explain the change before it is submitted.
- File issues only for bugs that were actually reproduced, using the bug template.
