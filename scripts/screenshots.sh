#!/usr/bin/env bash
# Re-render the README screenshots in docs/ from Raido's real UI (macOS).
#
# The scenes in src/shots.rs drive Raido with keys in a fresh clone of this
# repository (real shells, real clangd) and write each frame as HTML; headless
# Chrome photographs those pages at 2×. Needs JetBrainsMono Nerd Font in
# ~/Library/Fonts, clangd and rust-analyzer (for the editor's server names), and
# Google Chrome (or CHROME=/path/to/chrome).
#
#   scripts/screenshots.sh            every screenshot
#   scripts/screenshots.sh lsp        only the scenes whose name contains "lsp"
set -euo pipefail
cd "$(dirname "$0")/.."
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[ -x "$chrome" ] || { echo "Chrome not found; set CHROME=/path/to/chrome" >&2; exit 1; }

rm -rf target/shots
cargo test --release --quiet "shots::shots_${1:-}" -- --ignored --test-threads=1

for page in target/shots/*.html; do
    name="$(basename "$page" .html)"
    read -r width height <"target/shots/$name.size"
    "$chrome" --headless=new --disable-gpu --hide-scrollbars \
        --force-device-scale-factor=2 --window-size="$width,$height" \
        --virtual-time-budget=3000 --screenshot="$PWD/docs/$name.png" \
        "file://$PWD/$page" >/dev/null 2>&1
    # Smaller files when pngquant is around (256 colors, as terminals have few).
    if command -v pngquant >/dev/null; then
        pngquant --force --skip-if-larger --output "docs/$name.png" -- "docs/$name.png" || true
    fi
    echo "docs/$name.png"
done
