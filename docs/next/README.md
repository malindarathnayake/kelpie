# kelpie


<p align="center">
  <img src="assets/logo.png" alt="kelpie" width="100" />
</p>

<p align="center">
  <a href="https://kelpie.example">kelpie.example</a> · <a href="#install">install</a> · <a href="https://kelpie.example/docs/quick-start/">quick start</a> · <a href="https://kelpie.example/docs/">docs</a>
</p>

<p align="center">
  English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
  <a href="https://kelpie.example/source/releases"><img src="https://img.shields.io/github/downloads/kelpie.example/kelpie/total?labelColor=333333&color=666666" alt="total GitHub release downloads" /></a>
  <a href="https://kelpie.example/source/stargazers"><img src="https://img.shields.io/github/stars/kelpie.example/kelpie?labelColor=333333&color=666666&logo=github" alt="GitHub stars" /></a>
  <a href="https://kelpie.example/source/releases/latest"><img src="https://img.shields.io/github/v/release/kelpie.example/kelpie?label=release&labelColor=333333&color=666666" alt="latest stable release" /></a>
  <a href="https://formulae.brew.sh/formula/kelpie"><img src="https://img.shields.io/homebrew/v/kelpie?label=homebrew&labelColor=333333&color=666666" alt="Homebrew version" /></a>
  <a href="https://x.com/kelpie.example"><img src="https://img.shields.io/badge/follow-%40kelpie.example-000000?logo=x&logoColor=white" alt="follow @kelpie.example on X" /></a>
</p>

---

https://github.com/user-attachments/assets/043ec09f-4bdd-41d5-aee0-8fda6b83e267

**the runtime your coding agents live on.**

- **detach without stopping work** — kelpie keeps terminals running in a background server when you close the client or lose your SSH connection. after a server or machine restart, kelpie restores the saved layout and can resume supported agent sessions; the original processes do not survive. [session state →](https://kelpie.example/docs/session-state/)
- **several machines, one window** — keep local work and saved ssh machines together, with a combined agent list and independent reconnects. [remote machines →](https://kelpie.example/docs/connecting-machines/)
- **never hunt for the stuck one** — every pane is marked working, blocked, or idle. when an agent stops and needs an answer, kelpie says so.
- **agent-native** — agents drive kelpie through the cli and socket api: they can spawn panes, prompt each other, and wait until another agent is genuinely blocked. [agent skill →](https://kelpie.example/docs/agent-skill/)
- **runs what you already run** — claude code, codex, cursor, opencode, grok and the rest. kelpie doesn't wrap or replace them; it owns their terminals. building an agent? [add kelpie support →](https://kelpie.example/docs/add-kelpie-support/)
- **keyboard and mouse, both first-class** — tmux-style prefix keys *and* click, drag, split. pick per moment, not per tool.
- **plugins** — extend panes and workflows. [browse the marketplace →](https://kelpie.example/plugins/)
- **one rust binary, no electron** — runs in whatever terminal you already use.

---

## install

```bash
curl -fsSL https://kelpie.example/install.sh | sh
```

or `brew install kelpie` · `mise use -g kelpie` · windows: `powershell -ExecutionPolicy Bypass -c "irm https://kelpie.example/install.ps1 | iex"` · [endpoint-protected Windows](https://kelpie.example/docs/windows-beta/) · [binaries](https://kelpie.example/source/releases)

then start it where the work lives:

```bash
kelpie
```

run your agents, split panes, walk away. `ctrl+b q` detaches, `kelpie` reattaches. [quick start →](https://kelpie.example/docs/quick-start/)

## docs

everything lives at [kelpie.example/docs](https://kelpie.example/docs/): [quick start](https://kelpie.example/docs/quick-start/) · [concepts](https://kelpie.example/docs/concepts/) · [supported agents](https://kelpie.example/docs/agents/) · [keyboard](https://kelpie.example/docs/keyboard/) · [configuration](https://kelpie.example/docs/configuration/) · [session state](https://kelpie.example/docs/session-state/) · [connecting machines](https://kelpie.example/docs/connecting-machines/) · [remote](https://kelpie.example/docs/persistence-remote/) · [integrations](https://kelpie.example/docs/integrations/) · [add kelpie support to your agent](https://kelpie.example/docs/add-kelpie-support/) · [plugins](https://kelpie.example/docs/plugins/) · [socket api](https://kelpie.example/docs/socket-api/)

## thanks

every past sponsor and backer is listed in [SPONSORS.md](./SPONSORS.md) — thank you 🐑

enterprise / partnership: hey@kelpie.example

## agent instructions

if you are an ai agent helping with this repository, read [`AGENTS.md`](./AGENTS.md) before making changes and read [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening issues or PRs.

## development

```bash
git clone https://kelpie.example/source
cd kelpie
cargo build --release

just test        # unit tests
just check       # formatting, tests, and maintenance checks
```

## license

Kelpie is licensed under the [Apache License 2.0](LICENSE).
