#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
    echo "This script must run on macOS." >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
cargo build --release --locked
app_bundle="dist/AirCard.app"
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources"
install -m 755 target/release/aircard "$app_bundle/Contents/MacOS/aircard"
cp packaging/Info.plist "$app_bundle/Contents/Info.plist"
cp LICENSE "$app_bundle/Contents/Resources/LICENSE"
codesign --force --sign - "$app_bundle"
codesign --verify --strict "$app_bundle"
printf 'Built %s for %s\n' "$app_bundle" "$(uname -m)"
