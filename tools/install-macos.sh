#!/bin/sh
# Installs Vellum for the current user (no admin rights needed):
#   - builds a release Vellum.app bundle into ~/Applications
#   - registers it as a viewer for PDFs (Finder "Open With", Get Info > Open with)
# Undo by deleting ~/Applications/Vellum.app.
# Usage: tools/install-macos.sh [--no-build]
set -eu

project=$(cd "$(dirname "$0")/.." && pwd)
cd "$project"

[ "${1:-}" = "--no-build" ] || cargo build --release

target=$(cargo metadata --format-version 1 --no-deps | sed -E 's/.*"target_directory":"([^"]*)".*/\1/')
built="$target/release/vellum"
[ -x "$built" ] || { echo "Release build not found at $built" >&2; exit 1; }
version=$(sed -nE 's/^version = "(.*)"/\1/p' Cargo.toml | head -n 1)

app="$HOME/Applications/Vellum.app"
pkill -f "$app/Contents/MacOS/Vellum" 2>/dev/null || true
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$built" "$app/Contents/MacOS/Vellum"
cp assets/vellum.icns "$app/Contents/Resources/Vellum.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Vellum</string>
    <key>CFBundleDisplayName</key><string>Vellum</string>
    <key>CFBundleIdentifier</key><string>io.github.turbovadim.vellum</string>
    <key>CFBundleExecutable</key><string>Vellum</string>
    <key>CFBundleIconFile</key><string>Vellum</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>PDF Document</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>com.adobe.pdf</string></array>
        </dict>
    </array>
</dict>
</plist>
EOF

# Ad-hoc sign the bundle so Gatekeeper and LaunchServices accept it as a whole.
codesign --force --sign - "$app"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$app"

echo "Installed Vellum to $app"
echo "Right-click any PDF > Open With > Vellum. To make it the default, use Get Info > Open with > Change All."
