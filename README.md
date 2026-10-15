<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/kelpie-lockup-horizontal-dark.svg" />
    <img src="assets/brand/kelpie-lockup-horizontal-light.svg" alt="Kelpie" width="400" />
  </picture>
</p>

<p align="center"><b>Many agents. One calm shepherd's dog.</b></p>

<p align="center">
  <a href="#install">Install</a> · <a href="#what-it-does">What it does</a> · <a href="#development">Development</a> · <a href="#credits">Credits</a> · English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-d9a066?labelColor=1a1a24" alt="Apache 2.0 license" /></a>
  <img src="https://img.shields.io/badge/Windows%2011-first--class-d9a066?labelColor=1a1a24" alt="First-class on Windows 11" />
  <img src="https://img.shields.io/badge/built%20on-herdr-d9a066?labelColor=1a1a24" alt="Built on herdr" />
</p>

```
▗▖ ▗▖
▐▙▄▟▌     Kelpie
███▘███▄  agent workspace
███████▘  built on herdr
▐██▌
```

---

Kelpie is a terminal multiplexer and runtime for AI coding agents such as Claude Code, Codex and Cursor. Run ten or more sessions side by side, each with its own native terminal UI, backed by a server-owned runtime with a local JSON API. A supervisor agent can watch every session and nudge it forward. The human always wins. First-class on Windows 11 and PowerShell 7, and at home on Linux and macOS.

## What it does

- **Detach without stopping work.** Terminals keep running in a background server when you close the client or lose your SSH connection. After a restart, Kelpie restores the saved layout and can resume supported agent sessions.
- **Every agent in view.** Each pane is marked working, waiting, idle or done. The sidebar shows how long a pane has been idle, flags stale workspaces and marks the one waiting for you with `!!`.
- **A supervisor that asks before it types.** `kelpie agent watch` streams a bounded view of every agent. `kelpie agent submit`, `answer`, `interrupt`, `reset` and `resume` write only into a clean input box, are fenced against your own keystrokes, and leave a receipt you can check with `kelpie agent submit-status`.
- **Agent profiles.** Claude Code, Codex and Cursor are built in: input box shape, placeholder style, submit, interrupt and clear keys. Claude Code also confirms delivery through its prompt hook.
- **Runs what you already run.** Claude Code, Codex, Cursor, OpenCode, Grok and more. Kelpie does not wrap or replace them; it owns their terminals.
- **Keyboard and mouse, both first-class.** tmux-style prefix keys (`ctrl+b`) and click, drag, split.
- **Calm themes for long sessions.** `midnight`, `dusk`, `ember`, `slate`, `forest` and `kelpie`, with a live picker on `ctrl+b t`.
- **Host facts.** `ctrl+b i` shows the machine, shell, GPU, Docker, WSL and the command-line tools agents can use.
- **One Rust binary, no Electron.** Runs in whatever terminal you already use.

## Install

Kelpie has no release feed yet, so build it from source. Self-update is switched off until it does.

Requirements: Rust (the version in `rust-toolchain.toml`) and Zig 0.16.0.

```powershell
git clone <kelpie repository>
cd kelpie
cargo build --release
.\target\release\kelpie.exe
```

On Linux and macOS the binary is `./target/release/kelpie`.

Kelpie keeps its own config, state and socket, so it runs side by side with an installed herdr without touching it.

Run your agents, split panes, walk away. `ctrl+b q` detaches and `kelpie` reattaches.

## Development

```bash
just test        # unit tests
just check       # formatting, tests and maintenance checks
```

If you are an AI agent working in this repository, read [`AGENTS.md`](./AGENTS.md) before making changes and [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening issues or PRs.

## Credits

Kelpie is a community fork built on [herdr](https://github.com/herdrdev/herdr) by Ogulcan Celik and its contributors. The runtime, the terminal engine, the agent detection and most of what makes Kelpie work started there. Thank you.

herdr's past sponsors and backers are listed in [SPONSORS.md](./SPONSORS.md).

## License

Kelpie is licensed under the [Apache License 2.0](LICENSE), the same license as herdr.
