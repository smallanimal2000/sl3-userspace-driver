#!/bin/bash
# Install the Rane SL3 userland audio driver: the Rust sl3d daemon (owns the USB
# device) + the SL3.driver AudioServerPlugin (exposes it to CoreAudio). The plugin
# is C; the daemon is Rust. They talk over the shared-memory ABI.
#
# Requires sudo: copies into /Library and /usr/local, and restarts coreaudiod
# (this briefly interrupts ALL system audio).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
PLUGIN_BIN="$REPO/bazel-bin/coreaudio/SL3"
DAEMON="$REPO/bazel-bin/driver/sl3d/sl3d"

# Build via bazel (as the invoking user so bazel/brew are on PATH).
run_user() { if [ "$(id -u)" = "0" ] && [ -n "${SUDO_USER:-}" ]; then sudo -u "$SUDO_USER" "$@"; else "$@"; fi; }
run_user sh -c "cd '$REPO' && bazel build //coreaudio:SL3 //driver/sl3d:sl3d"

[ -f "$PLUGIN_BIN" ] || { echo "missing $PLUGIN_BIN"; exit 1; }
[ -x "$DAEMON" ] || { echo "missing $DAEMON"; exit 1; }

# Assemble the SL3.driver bundle from the bazel-built executable + Info.plist.
PLUGIN="$REPO/dist/SL3.driver"
rm -rf "$PLUGIN"; mkdir -p "$PLUGIN/Contents/MacOS"
cp "$PLUGIN_BIN" "$PLUGIN/Contents/MacOS/SL3"; chmod u+w "$PLUGIN/Contents/MacOS/SL3"
cp "$REPO/coreaudio/Info.plist" "$PLUGIN/Contents/Info.plist"

if [ "$(id -u)" != "0" ]; then echo "re-running under sudo..."; exec sudo "$0" "$@"; fi

echo "==> ad-hoc signing plugin"
codesign --force --sign - "$PLUGIN"

echo "==> installing daemon -> /usr/local/bin/sl3d"
install -m 0755 "$DAEMON" /usr/local/bin/sl3d

echo "==> installing LaunchDaemon"
install -m 0644 "$REPO/packaging/com.rane.sl3d.plist" /Library/LaunchDaemons/com.rane.sl3d.plist
launchctl bootout system/com.rane.sl3d 2>/dev/null || true
# Kill any stray daemons (e.g. a hand-run one) so the launchd instance has
# exclusive access to the USB device — two daemons fight and neither streams.
pkill -9 -f '/usr/local/bin/sl3d' 2>/dev/null || true
launchctl enable system/com.rane.sl3d 2>/dev/null || true
launchctl bootstrap system /Library/LaunchDaemons/com.rane.sl3d.plist

echo "==> installing plugin -> /Library/Audio/Plug-Ins/HAL/SL3.driver"
rm -rf /Library/Audio/Plug-Ins/HAL/SL3.driver
cp -R "$PLUGIN" /Library/Audio/Plug-Ins/HAL/SL3.driver
chown -R root:wheel /Library/Audio/Plug-Ins/HAL/SL3.driver

echo "==> restarting coreaudiod (system audio will glitch briefly)"
killall coreaudiod 2>/dev/null || true

cat <<'EOF'

  ============================================================================
   IMPORTANT: restarting coreaudiod reverts macOS's default output to your
   built-in speakers. Until you select "Rane SL 3" as the OUTPUT device,
   playback will go to the speakers and the SL 3 will (correctly) look silent.

     System Settings > Sound > Output  ->  Rane SL 3
     (or Audio MIDI Setup > Rane SL 3 > "Use this device for sound output")

   Verify the device exists:  system_profiler SPAudioDataType | grep 'Rane SL 3'
  ============================================================================

EOF
echo "done."
