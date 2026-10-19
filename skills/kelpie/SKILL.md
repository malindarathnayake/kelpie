---
name: kelpie
description: "Control Kelpie, a terminal multiplexer for coding agents. Use only when the user explicitly mentions Kelpie or asks to use Kelpie to inspect or control panes, tabs, workspaces, commands, or another agent. Do not use merely because a task could benefit from a background terminal, delegation, or parallel work. Requires KELPIE_ENV=1."
---

# Kelpie

Kelpie organizes terminals into workspaces, tabs, and panes, recognizes coding agents running inside panes, and exposes the current session through the `kelpie` CLI.

Before issuing any control command, verify that this agent is running inside a Kelpie-managed pane:

```bash
test "${KELPIE_ENV:-}" = 1
```

If the check fails, say that you are not running inside Kelpie and stop. Do not inspect or control the focused Kelpie session from outside Kelpie.

When the check passes, the `kelpie` binary in `PATH` talks to the current session. Use it to inspect neighboring work, create terminal layout, start agents and commands, read output, and wait for state changes.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
kelpie --help
```

Then print the relevant command group by running the group without a subcommand:

```bash
kelpie agent
kelpie pane
kelpie workspace
kelpie tab
kelpie worktree
kelpie terminal
kelpie notification
kelpie integration
kelpie session
kelpie machine
```

Do not run bare `kelpie` for discovery; it launches or attaches the TUI. Do not probe a mutating nested command by omitting arguments. Commands such as `kelpie workspace create` are valid with defaults and will execute.

Most control commands return JSON. Read identifiers and state from those responses instead of predicting them.

## Understand layout, panes, and agents

Choose the primitive that matches the job:

- Workspace, tab, and pane topology organize terminal locations.
- Pane commands control raw terminals, shells, tests, servers, input, and output.
- Agent commands control the recognized coding agent currently occupying a pane.

A pane exists whether or not it contains an agent. `agent start` requires an existing available shell pane and never creates, splits, or moves layout. Use pane commands for ordinary processes. Use agent commands when Kelpie must validate agent identity or interpret `idle`, `working`, `blocked`, `done`, and `unknown` lifecycle states.

Agent commands accept either a unique live agent name or the pane ID currently hosting that agent. They do not accept terminal IDs or bare agent-kind labels. Names must match `[a-z][a-z0-9_-]{0,31}` and be unique among live agents. A name follows the current pane occupant and is cleared when that agent exits, is released, or is replaced.

`idle` and `done` both mean the agent is ready for input. The CLI/API uses the server's seen state to distinguish them; explicit focus commands mark the target seen, while reads do not. Each TUI client tracks viewed completions independently, so its Done badge can differ from the CLI or another client's badge. `blocked` means Kelpie recognized an approval or question UI. `unknown` means an agent is present but Kelpie cannot classify it confidently; it does not prove completion.

## Use IDs and caller context

Public IDs are opaque stable handles:

- workspace: `w1`
- tab: `w1:t1`
- pane: `w1:p1`

Closed tab and pane IDs are not reused. A pane moved into another workspace receives a new workspace-qualified pane ID. After `pane move`, continue with `.result.move_result.pane.pane_id` or the live agent name. The old value is reported as `.result.move_result.previous_pane_id`; only the moved process's inherited caller context keeps resolving that old ID, so do not use it as a general agent target.

Kelpie injects the caller's context into each managed pane:

```bash
printf '%s\n' "$KELPIE_WORKSPACE_ID" "$KELPIE_TAB_ID" "$KELPIE_PANE_ID"
```

Prefer `--current` when a pane command should target the calling pane. An omitted `pane split` target uses the calling pane when `KELPIE_PANE_ID` is available, otherwise the focused pane. Other commands may use the UI-focused pane, which can belong to the user or another client.

Discover live state with:

```bash
kelpie workspace list
kelpie tab list --workspace "$KELPIE_WORKSPACE_ID"
kelpie pane current --current
kelpie pane list --workspace "$KELPIE_WORKSPACE_ID"
kelpie agent list
```

Creation responses expose the IDs to use next. `workspace create` returns `.result.workspace`, `.result.tab`, and `.result.root_pane`. `tab create` returns `.result.tab` and `.result.root_pane`. `pane split` returns the new pane as `.result.pane`.

IDs and live agent names are scoped to one server. Two saved SSH machines can both have `w1:p1` or an agent named `reviewer`. Selecting a machine in the TUI does not retarget commands running in your pane: without `--machine`, they still use the inherited session and socket context.

To control a saved SSH machine, use the same global prefix for discovery and every later command:

```bash
kelpie --machine <label-or-id> agent list
kelpie --machine <label-or-id> pane list
kelpie --machine <label-or-id> agent digest
```

The selector must be an enabled saved profile ID or a unique, case-sensitive label, not an arbitrary SSH hostname. Commands use that profile's remote session without an open TUI. Do not combine `--machine` with `--session` or `--remote`. Discover IDs on that machine; inherited local IDs and `--current` do not identify remote panes.

Both installations must support machine API forwarding, and the remote server must already be running and API-compatible. Forwarding never installs, starts, or restarts a server and never falls back to Local. Local configuration, session management, installation commands, and interactive attachment are not forwarded. Remote worktree paths must be absolute, `~`, or start with `~/`; plugin link paths must be absolute. A connection failure does not prove a mutation was not applied: inspect remote state before retrying.

`kelpie machine list` lists saved connection profiles, not a cross-machine pane inventory; add `--json` for scripts. Only add, remove, enable, or disable profiles when the user asks. Removing a profile disconnects the client but does not stop remote sessions. Interactive `machine add` discovers running remote sessions and may ask the user to pick one; non-interactive setup uses the remote default session unless `--remote-session` is explicitly supplied. Use `kelpie machine status [<label-or-id>] --json` to diagnose a failing machine. `kelpie machine reconnect` needs the user to complete SSH authentication in their terminal; do not run it for them. Setup asks before stopping an incompatible server and defaults to No; do not approve replacement without the user's consent. Experimental handoff is not part of `machine add`.

## Start and coordinate an agent

Default to a sibling pane in the current tab and the current working directory. Do not create a workspace, tab, worktree, or different cwd unless the user explicitly requests that topology or location.

Honor a direction requested by the user. Otherwise inspect the caller pane:

```bash
kelpie pane layout --pane "$KELPIE_PANE_ID"
```

Split a wide pane to the right and a narrow or tall pane down. Avoid repeated same-direction splits that create unusably narrow columns or short rows. Keep the user's focus in the calling pane and explicitly preserve the caller's working directory:

```bash
kelpie pane split --current --direction right --cwd "$PWD" --no-focus
```

Replace `right` with `down` when appropriate. Read the new pane ID from `.result.pane.pane_id`.

An available shell pane must be at its interactive prompt, with the shell itself in the foreground and no foreground command, editor, or agent running. Start a supported agent in that pane with a useful unique name:

```bash
kelpie agent start reviewer --kind codex --pane <returned-pane-id>
```

Use the kind requested by the user. Run `kelpie agent` to inspect the installed kind list and options. Pass native agent arguments only after `--`:

```bash
kelpie agent start reviewer --kind codex --pane <returned-pane-id> -- <agent-args...>
```

A successful `agent start` returns only after Kelpie detects the expected agent in the same pane and considers it ready for interactive input. If the agent is blocked during startup, the command returns `agent_not_ready` immediately but keeps the name available for `agent read`. Wait until the agent becomes idle before prompting it. Startup defaults to a 30-second timeout.

Supervised writes (`agent submit`, `answer`, `interrupt`, `reset`, `clear-draft`, `resume`) need `[supervisor] enabled = true` in the server's config. Each write names an exact target and an idempotency key. Read the target first:

```bash
kelpie agent digest
```

Copy the agent's complete `target_ref` (`<server_epoch>:<pane_id>:<generation>`) from the digest, pick a fresh submit ID, and submit:

```bash
kelpie agent submit --target-ref '<target_ref>' --submit-id review-20261019-01 "Review the current diff and report only actionable findings."
kelpie agent submit-status review-20261019-01
kelpie agent wait reviewer --timeout 120000
```

The receipt reports delivery (`written`, `fenced` or `indeterminate`), not that the agent started a turn. Repeating a submit ID returns the same receipt and never writes again, so retry with the same ID after a lost response; use a new ID only for new work. A changed generation or a server restart returns `stale_target`: read the digest again. `agent_blocked` means the agent waits at an approval or question dialog; inspect it and ask the user before answering. Answer an open prose question with `agent answer --question-id` from the digest; stop a working agent with `agent interrupt`.

`draft_not_clean` protects the user's own typing. When the message says the owner typed in this pane, no retry, wait, or agent restart will help: once the user types in a pane, a bot may not write to it until the user hands it over. Ask the user to focus that pane in their Kelpie window and press the delegate key (prefix, then Shift+A; Ctrl+B, then Shift+A by default) and pick "hand over once" or a delegation time. The pane shows "bot waiting" until they do. Never work around this with raw pane input. Any other `draft=dirty` or `draft=unknown` means the input box is not provably empty; read the pane and ask the user.

Without `--until`, `agent wait` matches the first settled `idle`, `done`, or `blocked` state. Use `--until` only for a state-specific workflow, such as waiting for an already-running agent to request input:

```bash
kelpie agent wait reviewer --until blocked --timeout 120000
```

Read the result through the resolved agent:

```bash
kelpie agent get reviewer
kelpie agent read reviewer --source recent-unwrapped --lines 120
```

If a wait fails or returns `blocked`, inspect `agent get` and `agent read` before deciding what input to send. A timeout does not prove the prompt was never delivered; check `agent submit-status` instead of submitting new work. Use the pane surface only when raw terminal control is intentional.

## Run an ordinary command in another pane

Create a sibling pane with the same geometry rule, preserve the caller's working directory, and keep user focus unchanged:

```bash
kelpie pane split --current --direction right --cwd "$PWD" --no-focus
```

Read the new pane ID from `.result.pane.pane_id`, then run and inspect the command:

```bash
kelpie pane run <returned-pane-id> "just test"
kelpie pane wait-output <returned-pane-id> --match "test result" --timeout 120000
kelpie pane read <returned-pane-id> --source recent-unwrapped --lines 120
```

`pane run` atomically sends command text and Enter. `pane wait-output` searches the selected snapshot immediately, so output that already exists can match. Use `--match <text>` for a literal substring or `--regex <pattern>` for a Rust regular expression. Omitting `--timeout` allows an indefinite wait.

Use the read source that matches the task:

- `visible`: the currently rendered viewport.
- `recent`: recent rendered output, including soft wraps.
- `recent-unwrapped`: recent output with soft wraps joined; prefer it for logs and transcripts.
- `detection`: the plain-text bottom-buffer snapshot used for agent detection.

Use `--format ansi` when colors and terminal styling are evidence. Otherwise use text.

`--lines` asks Kelpie for more rows from the pane's available screen and host scrollback. Alternate-screen rows do not enter ordinary host scrollback. For supported idle agents, Kelpie can collect application-owned history and restore the viewport afterward, but not every application or response can be recovered this way.

If a larger recent read still does not reveal the completed response, ask the agent to write it as Markdown in a temporary directory and reply only with the file path, then read that file on the same machine. Use this only as a fallback; do not request file output in the initial prompt.

## Safety and coordination rules

- Use `--no-focus` for background work unless the user asked to switch context.
- Use `--current`, an explicit pane ID, or a unique agent name. Do not rely on another client's focused pane.
- Parse IDs from JSON responses. Do not derive them from sidebar order or examples.
- Do not close workspaces, tabs, panes, or sessions you did not create unless the user explicitly asked. `workspace close --group` closes the primary workspace and its linked worktree workspaces; never add it merely to bypass `workspace_group_close_required`.
- Use `--trust-repository` only after the user has verified the repository. It grants per-request Git trust; it is not a routine retry for a failed worktree command.
- Client and server versions can differ after an update. Check `kelpie status` before relying on new server features. A missing method is not permission to stop or upgrade a server.
- Never run `kelpie server stop` from an active session unless the user explicitly intends to stop the server and its pane processes.
- Never kill the main Kelpie process. Use named test sessions for experiments that need an isolated server.
- CLI server errors are JSON on stderr with exit status 1. CLI syntax errors exit with status 2.
