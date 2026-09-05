#!/bin/bash
# Assemble a macOS Sl3Api.framework from the Bazel-built cdylib, with the same
# install-name the shipped prefPane imports. Output: dist/Sl3Api.framework
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"

echo "==> building cdylib (bazel)"
( cd "$REPO" && bazel build //api:Sl3Api )

DYLIB="$REPO/bazel-bin/api/libSl3Api.dylib"
FW="$REPO/dist/Sl3Api.framework"
INSTALL_NAME="/Library/Frameworks/Sl3Api.framework/Versions/A/Sl3Api"

rm -rf "$FW"
mkdir -p "$FW/Versions/A/Resources"
cp "$DYLIB" "$FW/Versions/A/Sl3Api"
chmod u+w "$FW/Versions/A/Sl3Api"   # bazel outputs are read-only
install_name_tool -id "$INSTALL_NAME" "$FW/Versions/A/Sl3Api"

cat > "$FW/Versions/A/Resources/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Sl3Api</string>
	<key>CFBundleIdentifier</key><string>com.rane.Sl3Api</string>
	<key>CFBundlePackageType</key><string>FMWK</string>
	<key>CFBundleExecutable</key><string>Sl3Api</string>
	<key>CFBundleShortVersionString</key><string>2.0.1</string>
	<key>CFBundleVersion</key><string>2.0.1</string>
</dict>
</plist>
PLIST

ln -sf A "$FW/Versions/Current"
ln -sf Versions/Current/Sl3Api "$FW/Sl3Api"
ln -sf Versions/Current/Resources "$FW/Resources"

echo "built $FW"
otool -D "$FW/Sl3Api" | tail -1
