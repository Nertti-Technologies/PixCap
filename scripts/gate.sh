#!/bin/sh
# The local gate: run before every commit and every release build.
#
#   1. Rust tests and Clippy (unwrap/expect are denied in non-test code).
#   2. Licence policy: every shipped Rust crate under an allowed licence (deny.toml).
#   3. Licence files: THIRD-PARTY-NOTICES.txt and the Windows installer licence page
#      match what scripts/notices.sh generates.
#   4. macOS app build and self-test, when run on a Mac.
#
# The Windows app is not built here; it needs a Windows machine (see
# platforms/windows/README.md).
set -eu
cd "$(dirname "$0")/.."

echo "▸ Rust tests"
cargo test --workspace --quiet

echo "▸ Clippy"
cargo clippy --workspace --all-targets --quiet

echo "▸ Licence policy"
cargo deny check licenses sources

echo "▸ Licence files"
scripts/notices.sh --check

if [ "$(uname)" = "Darwin" ]; then
    echo "▸ macOS build and self-test"
    cargo build -p pixcap-ffi --quiet
    (cd platforms/macos/PixCapMac && swift build && .build/debug/PixCapMac --self-test)
fi

echo "Gate passed."
