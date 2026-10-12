# Claude Design prompt: Kelpie brand kit

Paste everything below the line into Claude Design.

---

Design a complete brand kit for **Kelpie**, an open-source terminal workspace for running and supervising many AI coding agents at once. Treat this as a public developer-tool launch. The output should be a brand guidelines document plus production-ready assets.

## What Kelpie is
- A terminal multiplexer and runtime for AI coding agents (Claude Code, Codex, pi, grok and others). You run 10+ agent sessions side by side, and each one keeps its own native terminal UI.
- A server-owned runtime with a local JSON API. The terminal UI (TUI) is one client. Plugins and a team SDK extend it.
- Its signature feature: a **supervisor agent** (e.g. OpenAI's ChatGPT agent, Hermes, OpenClaw, or a script) can watch every session and nudge them forward. Permissions are per project (observe / assist / autopilot), and **the human always wins**.
- First-class on **Windows 11 and PowerShell 7**, and it works on Linux and macOS too.
- Kelpie is a community fork built on the open-source project **herdr**. The brand must be clearly its own and must not imply endorsement by herdr or its authors. The guidelines need a respectful "built on herdr" attribution line.

## Name and story
- **Kelpie**: the Australian working sheepdog. A kelpie handles a huge flock with very little direction, reads the whole field at once, and brings strays back without fuss. That is the product: one calm, alert helper keeping many agents moving, while the shepherd (the developer) stays in charge.
- Use the **dog** meaning only. Avoid anything that evokes the Scottish water-horse spirit of the same name (no horses, no water monsters, no Celtic folklore styling).
- CLI command: `kelpie`, always lowercase in code contexts.

## Audience and personality
- Audience: professional developers, platform/DevOps engineers and AI-tooling teams; Windows-heavy, terminal-literate.
- Personality: **alert, capable, calm, trustworthy, quietly playful**. Think "a dog that knows its job", not "cute mascot". Confident but never loud or hype-y.
- Avoid: generic AI clichés (neural-network brains, sparkles, robot faces, purple-gradient "AI glow"), cartoon puppies, sheep-counting jokes as the main motif, aggressive or militaristic "control" imagery.

## Hard constraints (the brand lives in a terminal)
1. **It must work in text.** Supply a version of the mark and the wordmark built from Unicode text characters that fits in **≤ 6 rows × ≤ 24 columns** for a TUI splash screen and CLI `--help` header. Also supply a one-line version (≤ 12 columns, e.g. a glyph plus `kelpie`). Use only characters that render in Cascadia Mono, Consolas, JetBrains Mono and Menlo, and avoid emoji.
2. **Colour must degrade.** Every colour use has to work in truecolor, xterm-256, **16-colour ANSI** and monochrome. Never rely on colour alone to carry meaning; pair it with a glyph or text.
3. **Status semantics are fixed** (they appear as tokens in the product). Design the semantic palette around them and give each a matching glyph that can stand in for colour:
   - working: yellow family
   - waiting for you / needs attention: red/rose family (this one gets the strongest emphasis)
   - done / idle: green family
   - background job running: yellow, with a distinct secondary glyph
   - stale (idle too long): amber/peach, plus a non-colour cue
   - unknown / unavailable: dimmed neutral
   - supervisor / autopilot: mauve/violet accent
   Today's product uses Catppuccin as the default theme (Mocha dark: blue `#89b4fa`, mauve `#cba6f7`, green `#a6e3a1`, yellow `#f9e2af`, red `#f38ba8`, peach `#fab387`, teal `#94e2d5`, base `#1e1e2e`; Latte for light). The brand palette should sit comfortably next to Catppuccin, though it doesn't have to be Catppuccin.
4. **Dark first, light supported.** Most users run dark terminals. Every asset needs dark and light variants. Text contrast must meet WCAG AA, with 4.5:1 for body text.
5. **Small sizes matter.** The mark must read at **16 × 16 px** (favicon, Windows taskbar) and in a single terminal cell height.
6. **Distinct from upstream.** Do not reuse or echo herdr's existing logo. It is a dark monochrome geometric mark on a light grey square.

## Deliverables
1. **Logo system**
   - Primary mark: a kelpie expressed simply. Ideas to explore: an alert head in profile with ears up; a dog mid-turn around a flock of small squares/terminal panes; a negative-space dog inside a terminal-window shape. Show **3 distinct directions**, then refine the strongest.
   - Wordmark `kelpie` in lowercase, plus a lockup of mark and wordmark (horizontal and stacked).
   - Variants: full colour, single colour, inverted, monochrome line version, and the terminal text versions (multi-line and one-line).
   - Clear space, minimum sizes and misuse examples.
2. **App icons**: Windows (`.ico` at 16, 24, 32, 48, 64, 128, 256), macOS app icon, Linux 256/512 PNG, favicon (SVG + 32 px PNG), and a terminal tab/window title glyph.
3. **Colour**
   - Brand colours with hex, RGB, nearest xterm-256 index and 16-colour ANSI fallback.
   - The semantic status tokens above, for dark and light, with contrast ratios.
   - Deliver as **design tokens**: a JSON file (W3C design-tokens format) and CSS custom properties, plus a TOML snippet shaped like a terminal theme: `[theme.custom] accent = "#…"`, with keys `accent, panel_bg, text, green, yellow, red, blue, teal, peach, mauve`.
4. **Typography**: a monospace family for product, code and terminal surfaces, plus a display/heading family for the website and README. Both must be free, open-licensed and self-hostable. Show a type scale.
5. **Status iconography**: a small glyph set (both Unicode-text and SVG versions) for working, waiting-for-you, done, background-running, stale, unknown, supervisor-observe / assist / autopilot, and "human override / paused". These must be distinguishable in monochrome.
6. **Marketing surfaces**
   - GitHub README hero banner (dark and light, about 1280 × 320).
   - GitHub social preview (1280 × 640).
   - A simple docs/landing page header.
   - A mock **TUI screenshot frame** showing the brand applied: sidebar with workspace rows, a waiting row highlighted, and the supervisor badge in the status bar.
7. **Voice and tone**
   - 5 tagline options. One direction to try: "Many agents. One calm shepherd's dog." Also give alternatives without the dog.
   - A one-sentence and a one-paragraph description.
   - Writing rules (plain, specific, no hype; the human is in charge).
   - Example CLI messages: success, waiting-for-you, error.
8. **Brand guidelines document** pulling all of the above together, including the "built on herdr" attribution guidance and do/don't examples.

## Format
- Vector sources as SVG, plus PNG exports at 1× and 2×.
- Tokens as JSON + CSS + TOML.
- Terminal versions as plain UTF-8 `.txt` files with each line's column width noted.
- Name files predictably, e.g. `kelpie-mark-dark.svg` and `kelpie-wordmark-light.svg`.

## Process
Start with the 3 logo directions and a palette proposal, and stop for feedback before producing the full kit.
