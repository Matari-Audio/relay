#!/usr/bin/env bash
# Universal (arm64 + x86_64) RELAY CLAP/VST3, signed, packaged as a .pkg,
# notarized and stapled. Same flow and secret names as KURV's
# scripts/build-macos-signed.sh. Runs on a macOS runner.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT_DIR="${1:-$ROOT_DIR/target/macos-release}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
NAME=RELAY

for name in \
  APPLE_APPLICATION_CERTIFICATE_P12_BASE64 \
  APPLE_INSTALLER_CERTIFICATE_P12_BASE64 \
  APPLE_CERTIFICATE_PASSWORD \
  APPLE_DEVELOPER_ID_APPLICATION \
  APPLE_DEVELOPER_ID_INSTALLER \
  APPLE_ID \
  APPLE_APP_SPECIFIC_PASSWORD \
  APPLE_TEAM_ID; do
  [[ -n "${!name:-}" ]] || { echo "Missing required environment variable: $name" >&2; exit 1; }
done

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT_DIR/plugin/Cargo.toml" | head -1)"
[[ -n "$version" ]] || { echo "plugin version not found" >&2; exit 1; }

build_bundle() {
  local target="$1" target_dir="$ROOT_DIR/target/macos-$1"
  rm -rf -- "$target_dir"
  (cd "$ROOT_DIR/plugin" && CARGO_TARGET_DIR="$target_dir" cargo truce build \
    --clap --vst3 --target "$target" --target-cpu baseline)
  for bundle in $NAME.clap $NAME.vst3; do
    [[ -f "$target_dir/bundles/$target/$bundle/Contents/MacOS/$NAME" ]] || {
      echo "Missing $target $bundle binary" >&2; exit 1; }
  done
}

build_bundle aarch64-apple-darwin
build_bundle x86_64-apple-darwin

work_dir="$(mktemp -d)"
keychain="$work_dir/signing.keychain-db"
keychain_password="$(uuidgen)"
cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -rf -- "$work_dir"
}
trap cleanup EXIT

bundles="$work_dir/bundled"
arm="$ROOT_DIR/target/macos-aarch64-apple-darwin/bundles/aarch64-apple-darwin"
x86="$ROOT_DIR/target/macos-x86_64-apple-darwin/bundles/x86_64-apple-darwin"
mkdir -p -- "$bundles"
for bundle in $NAME.clap $NAME.vst3; do
  ditto "$arm/$bundle" "$bundles/$bundle"
  bin="$bundles/$bundle/Contents/MacOS/$NAME"
  lipo -create "$arm/$bundle/Contents/MacOS/$NAME" "$x86/$bundle/Contents/MacOS/$NAME" -output "$bin.tmp"
  mv -- "$bin.tmp" "$bin"
  lipo "$bin" -verify_arch arm64 x86_64
done

echo "$APPLE_APPLICATION_CERTIFICATE_P12_BASE64" | base64 --decode > "$work_dir/application.p12"
echo "$APPLE_INSTALLER_CERTIFICATE_P12_BASE64" | base64 --decode > "$work_dir/installer.p12"

security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$work_dir/application.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12
security import "$work_dir/installer.p12" -k "$keychain" -P "$APPLE_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12
security list-keychains -d user -s "$keychain"
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null

for bundle in $NAME.clap $NAME.vst3; do
  codesign --force --sign "$APPLE_DEVELOPER_ID_APPLICATION" \
    --keychain "$keychain" --options runtime --timestamp "$bundles/$bundle"
  codesign --verify --deep --strict --verbose=2 "$bundles/$bundle"
done

pkgroot="$work_dir/pkgroot"
mkdir -p "$pkgroot/Library/Audio/Plug-Ins/CLAP" "$pkgroot/Library/Audio/Plug-Ins/VST3"
ditto "$bundles/$NAME.clap" "$pkgroot/Library/Audio/Plug-Ins/CLAP/$NAME.clap"
ditto "$bundles/$NAME.vst3" "$pkgroot/Library/Audio/Plug-Ins/VST3/$NAME.vst3"

mkdir -p -- "$OUTPUT_DIR"
pkg="$OUTPUT_DIR/$NAME-${version}-macos.pkg"
pkgbuild --root "$pkgroot" --identifier com.matariaudio.relay.pkg --version "$version" \
  --install-location / --sign "$APPLE_DEVELOPER_ID_INSTALLER" --keychain "$keychain" "$pkg"

xcrun notarytool submit "$pkg" \
  --apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" --team-id "$APPLE_TEAM_ID" \
  --wait --output-format json > "$OUTPUT_DIR/$NAME-${version}-notary.json"
xcrun stapler staple "$pkg"
xcrun stapler validate "$pkg"
pkgutil --check-signature "$pkg"
spctl --assess --type install --verbose=4 "$pkg"
echo "$pkg"
