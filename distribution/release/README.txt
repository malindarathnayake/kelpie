Kelpie @VERSION@ - terminal workspace for running many AI coding agents side by side.
https://kelpie.multicastbits.com
https://github.com/malindarathnayake/kelpie

WINDOWS (x64)
  1. Unzip kelpie-windows-x64.zip.
  2. In PowerShell, in the unzipped folder:
       powershell -ExecutionPolicy Bypass -File .\install.ps1
     Add agent hooks at the same time with:
       powershell -ExecutionPolicy Bypass -File .\install.ps1 -Integrations claude,codex,cursor
  3. Open a new terminal and run:  kelpie
  Installs to %LOCALAPPDATA%\Programs\kelpie and adds it to your user PATH.
  Includes Microsoft's ConPTY (conpty.dll, OpenConsole.exe), signed by Microsoft.
  kelpie.exe is not code-signed yet, so SmartScreen or Defender may ask once.

MAC (Apple Silicon only)
  1. Extract kelpie-macos-arm64.tar.gz.
  2. In Terminal, in the extracted folder:
       sh install.sh
     Add agent hooks at the same time with:
       sh install.sh claude codex cursor
  3. Open a new terminal and run:  kelpie
  Installs to ~/.local/bin/kelpie. The binary is not notarized by Apple yet; the
  installer removes the download quarantine flag so macOS lets it run.

FIRST STEPS
  ctrl+b is the prefix key; the bar at the bottom lists what you can press next.
  ctrl+b s   settings (theme, sidebar marks, access, host facts)
  ctrl+b t   theme picker
  ctrl+b shift+a   hand the focused pane to your supervisor bot
  ctrl+b q   detach (agents keep running); run kelpie again to reattach
  ctrl+b ?   all keys

UPDATING
  Self-update is off for now. Download the latest release and run the
  installer again (stop Kelpie first with: kelpie server stop).

UNINSTALL
  kelpie integration uninstall <agent> for each hook, then delete the install
  folder and Kelpie's folders (Windows %APPDATA%\kelpie and
  %LOCALAPPDATA%\kelpie, Mac ~/.config/kelpie).

Kelpie is built on herdr (https://github.com/herdrdev/herdr). Apache-2.0.
