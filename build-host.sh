#!/bin/sh
set -eu
cd "$(dirname "$0")"

cargo build --release --bin flaky

if command -v codesign >/dev/null 2>&1; then
    codesign -s - --entitlements flaky.entitlements target/release/flaky
    echo "signed target/release/flaky"
else
    echo "codesign not found, skipping signature"
fi
