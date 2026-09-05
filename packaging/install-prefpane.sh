#!/bin/bash
# Install our drop-in Sl3Api.framework so the original
# "SL 3 Audio Control Panel.prefPane" drives the userland driver instead of the
# dead kext. Backs up the shipped Rane framework first. Requires sudo.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO/dist/Sl3Api.framework"
DEST="/Library/Frameworks/Sl3Api.framework"

# Build the Rust framework first (as the invoking user, so cargo/brew are on PATH).
if [ "$(id -u)" = "0" ] && [ -n "${SUDO_USER:-}" ]; then
  sudo -u "$SUDO_USER" "$REPO/packaging/build-framework.sh"
else
  "$REPO/packaging/build-framework.sh"
fi
[ -d "$SRC" ] || { echo "missing $SRC — build-framework.sh failed"; exit 1; }
if [ "$(id -u)" != "0" ]; then echo "re-running under sudo..."; exec sudo "$0" "$@"; fi

if [ -d "$DEST" ] && [ ! -d "$DEST.rane-orig" ]; then
  echo "==> backing up shipped framework -> $DEST.rane-orig"
  mv "$DEST" "$DEST.rane-orig"
fi

echo "==> installing our framework -> $DEST"
rm -rf "$DEST"
cp -R "$SRC" "$DEST"
chown -R root:wheel "$DEST"

echo "==> ad-hoc signing"
codesign --force --deep --sign - "$DEST"

echo "done."
echo "Open the control panel with:"
echo "  open \"/Library/PreferencePanes/SL 3 Audio Control Panel.prefPane\""
echo "To restore Rane's original:  sudo rm -rf \"$DEST\" && sudo mv \"$DEST.rane-orig\" \"$DEST\""
