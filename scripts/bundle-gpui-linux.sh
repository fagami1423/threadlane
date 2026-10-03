#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-target}"
version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "threadlane-gpui") | .version')"
architecture="$(uname -m)"
stage="$target_dir/release/threadlane-linux"
archive="$target_dir/release/Threadlane-${version}-linux-${architecture}.tar.gz"

cargo build --locked --release --bin threadlane-gpui
rm -rf "$stage" "$archive"
mkdir -p "$stage"

install -m755 "$target_dir/release/threadlane-gpui" "$stage/threadlane"
install -m644 resources/icon_512.png "$stage/Threadlane.png"
cat > "$stage/threadlane.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Threadlane
Exec=threadlane
Icon=Threadlane
Terminal=false
Categories=Development;
DESKTOP

tar -czf "$archive" -C "$stage" .
printf 'Created %s\n' "$archive"
