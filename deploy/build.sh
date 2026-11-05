#!/usr/bin/env bash
# Build the `fm` binary from the checkout in APP_DIR and install it as $APP_DIR/bin/fm (owned by
# root, like the code). Used by deploy/install.sh and deploy/update.sh; safe to run by hand:
#
#   sudo bash /opt/fantasy_manager/deploy/build.sh
#
# First run installs the Rust toolchain with rustup (stable, minimal profile) into
# /usr/local/rustup and /usr/local/cargo. Later runs update it. The build takes a few minutes on
# 2 vCPUs; on a 1 GB server add swap first (docs/hosting.md, "Low memory") and use BUILD_JOBS=1.
#
# Variables (optional): BUILD_JOBS (default 2), CARGO_TARGET_DIR (default $APP_DIR/target).
set -euo pipefail

APP_DIR="${APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
export PATH="$CARGO_HOME/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$APP_DIR/target}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "installing the Rust toolchain (rustup, stable) into $CARGO_HOME"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
elif command -v rustup >/dev/null 2>&1; then
  rustup update stable --no-self-update >/dev/null || echo "rustup update failed; building with $(rustc --version)"
fi

cd "$APP_DIR"
echo "building fm ($(rustc --version), ${BUILD_JOBS:-2} jobs)"
cargo build --release --locked -p fm-cli -j "${BUILD_JOBS:-2}"
install -d -m 755 "$APP_DIR/bin"
install -m 755 "$CARGO_TARGET_DIR/release/fm" "$APP_DIR/bin/fm.new"
mv -f "$APP_DIR/bin/fm.new" "$APP_DIR/bin/fm"     # atomic: a running job keeps the old binary
"$APP_DIR/bin/fm" --help >/dev/null && echo "fm installed: $APP_DIR/bin/fm"
