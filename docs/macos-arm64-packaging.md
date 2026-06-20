# macOS arm64 Packaging

This project currently uses a manual macOS packaging flow. It builds the
`air-desktop` release binary, places it in a minimal `.app` bundle, signs the
bundle with an ad-hoc local signature, and wraps it in a compressed DMG.

The resulting DMG is suitable for local testing on Apple Silicon Macs. It is
not notarized with Apple Developer ID, so Gatekeeper may warn on other machines.

## Prerequisites

- Apple Silicon Mac (`arm64`)
- Rust toolchain
- Xcode Command Line Tools
- Already-fetched Cargo and git dependencies, or network access for the first
  build

## Build

```bash
CARGO_NET_OFFLINE=true cargo build --release -p air-desktop --bin air
```

If dependencies or embedded mihomo assets are not cached yet, omit
`CARGO_NET_OFFLINE=true` for the first build.

## Create the App Bundle

```bash
rm -rf dist/Air.app
mkdir -p dist/Air.app/Contents/MacOS
mkdir -p dist/Air.app/Contents/Resources

cp target/release/air dist/Air.app/Contents/MacOS/Air
chmod +x dist/Air.app/Contents/MacOS/Air
cp crates/air-desktop/assets/app-icon.png dist/Air.app/Contents/Resources/app-icon.png

cat > dist/Air.app/Contents/Info.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Air</string>
  <key>CFBundleExecutable</key>
  <string>Air</string>
  <key>CFBundleIdentifier</key>
  <string>org.air.Air</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Air</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key>
  <true/>
</dict>
</plist>
PLIST
```

## Sign and Verify

```bash
codesign --force --deep --sign - dist/Air.app
codesign --verify --deep --strict dist/Air.app
```

## Create the DMG

```bash
rm -rf dist/dmg-root
mkdir -p dist/dmg-root
cp -R dist/Air.app dist/dmg-root/Air.app
ln -s /Applications dist/dmg-root/Applications

hdiutil create \
  -volname "Air" \
  -srcfolder dist/dmg-root \
  -ov \
  -format UDZO \
  dist/Air-macos-arm64.dmg
```

## Smoke Test

```bash
hdiutil attach dist/Air-macos-arm64.dmg
open /Volumes/Air/Air.app
```

After testing, eject the mounted volume:

```bash
hdiutil detach /Volumes/Air
```
