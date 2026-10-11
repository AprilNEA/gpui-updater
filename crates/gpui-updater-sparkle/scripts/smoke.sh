#!/usr/bin/env bash
# End-to-end smoke test of the Sparkle driver against the real Sparkle.framework.
#
# Builds the `smoke` example into two throwaway app bundles — one older and one
# newer than the 1.5.1 release in
# crates/gpui-updater-core/tests/fixtures/sparkle-2.10-generate_appcast.xml
# — serves that feed on 127.0.0.1, and checks that the driver reports
# `Found(1.5.1)` and `UpToDate` respectively.
#
# Usage: crates/gpui-updater-sparkle/scripts/smoke.sh [path/to/Sparkle.framework]
# Without an argument, downloads Sparkle 2.10.0 and checks its SHA-256.
#
# Sparkle records its check times under the `dev.gpui-updater.smoke` defaults
# domain; `defaults delete dev.gpui-updater.smoke` removes them.
set -euo pipefail

SPARKLE_VERSION=2.10.0
SPARKLE_SHA256=c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c
# Public half of the test key used by the Ed25519 tests in src/verify.rs.
PUBLIC_KEY="6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw="
PORT="${SMOKE_PORT:-8765}"

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
WORK="${SMOKE_WORK_DIR:-$REPO/target/sparkle-smoke}"
mkdir -p "$WORK"

if [ $# -ge 1 ]; then
  FRAMEWORK="$(cd "$1" && pwd)"
else
  ARCHIVE="$WORK/Sparkle-$SPARKLE_VERSION.tar.xz"
  if [ ! -f "$ARCHIVE" ]; then
    curl -fsSL -o "$ARCHIVE" \
      "https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VERSION/Sparkle-$SPARKLE_VERSION.tar.xz"
  fi
  echo "$SPARKLE_SHA256  $ARCHIVE" | shasum -a 256 -c - >/dev/null
  rm -rf "$WORK/sparkle"
  mkdir -p "$WORK/sparkle"
  tar -xJf "$ARCHIVE" -C "$WORK/sparkle"
  FRAMEWORK="$WORK/sparkle/Sparkle.framework"
fi

FEED_DIR="$WORK/feed"
mkdir -p "$FEED_DIR"
cp "$REPO/crates/gpui-updater-core/tests/fixtures/sparkle-2.10-generate_appcast.xml" "$FEED_DIR/appcast.xml"
FEED_URL="http://127.0.0.1:$PORT/appcast.xml"

(cd "$REPO" && cargo build -q -p gpui-updater-sparkle --example smoke)
SMOKE_BIN="$REPO/target/debug/examples/smoke"

# Sparkle only accepts http(s) feeds, so serve the fixture locally.
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$FEED_DIR" \
  >"$WORK/feed.log" 2>&1 &
SERVER=$!
trap 'kill "$SERVER" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
  curl -fs -o /dev/null "$FEED_URL" && break
  sleep 0.1
done

# bundle <version>: assemble and ad-hoc sign Smoke-<version>.app; print its path.
bundle() {
  local version="$1"
  local app="$WORK/Smoke-$version.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks"
  cp "$SMOKE_BIN" "$app/Contents/MacOS/smoke"
  ditto "$FRAMEWORK" "$app/Contents/Frameworks/Sparkle.framework"
  /usr/libexec/PlistBuddy \
    -c "Add :CFBundleIdentifier string dev.gpui-updater.smoke" \
    -c "Add :CFBundleName string Smoke" \
    -c "Add :CFBundleExecutable string smoke" \
    -c "Add :CFBundlePackageType string APPL" \
    -c "Add :CFBundleVersion string $version" \
    -c "Add :CFBundleShortVersionString string $version" \
    -c "Add :SUFeedURL string $FEED_URL" \
    -c "Add :SUPublicEDKey string $PUBLIC_KEY" \
    -c "Add :SUEnableAutomaticChecks bool false" \
    "$app/Contents/Info.plist" >/dev/null
  codesign --force --deep -s - "$app" 2>/dev/null
  echo "$app"
}

# expect <app version> <text the smoke output must contain>
expect() {
  local app output
  app="$(bundle "$1")"
  if ! output="$("$app/Contents/MacOS/smoke" 2>&1)"; then
    echo "$output"
    echo "sparkle smoke: Smoke-$1 failed" >&2
    exit 1
  fi
  echo "$output" | grep 'event' || true
  if ! grep -qF "$2" <<<"$output"; then
    echo "sparkle smoke: Smoke-$1 did not report: $2" >&2
    exit 1
  fi
}

expect 1.0.0 'event Found(Release { version: Version { major: 1, minor: 5, patch: 1 }'
expect 9.0.0 'event UpToDate'
echo "sparkle smoke: ok"
