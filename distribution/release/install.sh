#!/bin/sh
# Installs Kelpie for the current user into ~/.local/bin. No sudo needed.
#   sh install.sh
#   sh install.sh claude codex cursor     # also install those agent hooks
set -eu

here=$(cd "$(dirname "$0")" && pwd)
src="$here/kelpie"
dest_dir="$HOME/.local/bin"
dest="$dest_dir/kelpie"

[ -f "$src" ] || { echo "kelpie not found next to this script" >&2; exit 1; }
if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "This build is for Apple Silicon Macs (arm64) only." >&2
    exit 1
fi
if pgrep -x kelpie >/dev/null 2>&1; then
    echo "Kelpie is running. Run 'kelpie server stop', then run this again." >&2
    exit 1
fi

mkdir -p "$dest_dir"
cp "$src" "$dest"
chmod 755 "$dest"
# Not signed by Apple: drop the download quarantine flag so macOS lets it run.
xattr -d com.apple.quarantine "$dest" 2>/dev/null || true
echo "Installed $dest"

case ":$PATH:" in
    *":$dest_dir:"*) ;;
    *)
        profile="$HOME/.zprofile"
        echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$profile"
        echo "Added ~/.local/bin to PATH in $profile (open a new terminal)."
        ;;
esac

for agent in "$@"; do
    "$dest" integration install "$agent"
done

"$dest" --version
echo ""
echo "Next: open a NEW terminal and run:  kelpie"
echo "Agent hooks (optional, per agent):  kelpie integration install claude"
