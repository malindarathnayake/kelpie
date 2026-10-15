# Windows Terminal schemes for the long-session themes

Kelpie themes colour only Kelpie's own chrome: the sidebar, tab bar, overlays and
status markers. Everything inside a pane (Claude, shells, editors) is drawn with
your terminal's colour scheme. To make pane contents match, use the matching
Windows Terminal scheme from `windows-terminal-schemes.json`.

| Kelpie theme | Windows Terminal scheme |
|---|---|
| `midnight` | `Kelpie Midnight` |
| `dusk` | `Kelpie Dusk` |
| `ember` | `Kelpie Ember` (night use, minimal blue) |
| `slate` | `Kelpie Slate` |
| `forest` | `Kelpie Forest` |
| `kelpie` | `Kelpie Kelpie` |

Nothing here is applied automatically. Kelpie never writes to your Windows
Terminal settings.

## Add the schemes

1. In Windows Terminal open **Settings** (`Ctrl+,`) and select
   **Open JSON file** at the bottom left.
2. Find the `"schemes": [ ... ]` array. Paste the objects from
   `windows-terminal-schemes.json` inside it, separated from the existing
   entries by a comma. Paste the objects only, not the outer `[` and `]`.
3. Save the file. Windows Terminal reloads it straight away.

## Use a scheme

Set it per profile in the JSON file:

```json
"profiles": {
  "defaults": {
    "colorScheme": "Kelpie Midnight"
  }
}
```

or pick it under **Settings > Profiles > (profile) > Appearance > Color scheme**.

## Pick the Kelpie theme

- Inside Kelpie press `prefix+t` (default `ctrl+b t`) to open the theme picker.
  Moving the cursor previews the theme; Enter saves `theme.name`; Esc reverts.
- Or from a shell: `kelpie theme list` and `kelpie theme set midnight`.

## How the schemes are derived

Each scheme uses the Kelpie palette of the same name: background is `panel_bg`,
foreground is `text`, cursor is `accent` and selection is `selection_bg`. The
ANSI colours map `black` to `surface0`, `red`/`green`/`yellow`/`blue` to the
palette colours of the same name, `purple` to `mauve`, `cyan` to `teal`,
`white` to `subtext0` and `brightBlack` to `overlay0`. The other bright colours
are their normal colour mixed 12% towards white. No scheme uses pure black or
pure white.
