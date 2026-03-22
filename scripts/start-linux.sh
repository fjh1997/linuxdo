#!/usr/bin/env bash
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

cargo build --release
sudo ./target/release/linuxdo-accelerator start
