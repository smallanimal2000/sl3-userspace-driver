#!/bin/bash
# Remove the Rane SL3 userland driver. Requires sudo; restarts coreaudiod.
set -euo pipefail
if [ "$(id -u)" != "0" ]; then exec sudo "$0" "$@"; fi

launchctl unload /Library/LaunchDaemons/com.rane.sl3d.plist 2>/dev/null || true
rm -f /Library/LaunchDaemons/com.rane.sl3d.plist
rm -f /usr/local/bin/sl3d
rm -rf /Library/Audio/Plug-Ins/HAL/SL3.driver
# clean up the shared-memory object
rm -f /private/tmp/sl3_audio 2>/dev/null || true
killall coreaudiod 2>/dev/null || true
echo "uninstalled. (A reboot fully clears the /sl3_audio shared memory if it lingers.)"
