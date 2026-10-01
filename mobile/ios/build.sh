#!/usr/bin/env bash
# Build + run the Threadlane mobile client on an iOS simulator.
#
# Usage: ./build.sh [device-name]
#   ./build.sh                      # first available iPhone simulator
#   ./build.sh "iPhone 17"          # named simulator
set -euo pipefail

cd "$(dirname "$0")"

RUST_DIR="../rust"
TARGET_SIM="aarch64-apple-ios-sim"
BUNDLE_ID="dev.threadlane.mobile"
APP_NAME="ThreadlaneMobile"

# ── 1. Rust static library ────────────────────────────────────────────────────
rustup target add "$TARGET_SIM" >/dev/null 2>&1 || true
echo "==> cargo build --target $TARGET_SIM"
cargo build --manifest-path "$RUST_DIR/Cargo.toml" --lib --target "$TARGET_SIM"

# ── 2. Xcode project ──────────────────────────────────────────────────────────
if ! command -v xcodegen >/dev/null 2>&1; then
    echo "xcodegen not found — brew install xcodegen" >&2
    exit 1
fi
xcodegen

# ── 3. Simulator build ────────────────────────────────────────────────────────
DEVICE="${1:-}"
DESTINATION="platform=iOS Simulator"
if [ -n "$DEVICE" ]; then
    DESTINATION="platform=iOS Simulator,name=$DEVICE"
fi
echo "==> xcodebuild ($DESTINATION)"
xcodebuild \
    -project "$APP_NAME.xcodeproj" \
    -scheme "$APP_NAME" \
    -destination "$DESTINATION" \
    -configuration Debug \
    build | tail -20

# ── 4. Install + launch in a booted simulator ─────────────────────────────────
APP_PATH=$(find ~/Library/Developer/Xcode/DerivedData \
    -name "$APP_NAME.app" -path "*Debug-iphonesimulator*" -newer project.yml \
    | head -1)
if [ -z "$APP_PATH" ]; then
    echo "Could not locate built $APP_NAME.app under DerivedData" >&2
    exit 1
fi

if [ -z "$DEVICE" ]; then
    BOOTED=$(xcrun simctl list devices booted -j | python3 -c \
        'import json,sys; d=json.load(sys.stdin); print(next((u for devs in d["devices"].values() for u in devs if u["state"]=="Booted"), ""))')
    if [ -z "$BOOTED" ]; then
        # Boot the first iPhone we can find.
        BOOTED=$(xcrun simctl list devices available -j | python3 -c \
            'import json,sys; d=json.load(sys.stdin); print(next(u["udid"] for rt,devs in d["devices"].items() if "iPhone" in rt for u in devs))')
        xcrun simctl boot "$BOOTED"
        open -a Simulator
    fi
else
    BOOTED=$(xcrun simctl list devices available -j | python3 -c \
        "import json,sys; d=json.load(sys.stdin); print(next(u[\"udid\"] for devs in d[\"devices\"].values() for u in devs if u[\"name\"]==\"$DEVICE\"))")
    xcrun simctl boot "$BOOTED" 2>/dev/null || true
    open -a Simulator
fi

xcrun simctl install "$BOOTED" "$APP_PATH"
xcrun simctl launch "$BOOTED" "$BUNDLE_ID"
echo "==> Launched $BUNDLE_ID on $BOOTED"
