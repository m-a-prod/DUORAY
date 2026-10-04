#!/bin/sh
# Regenerates THIRD-PARTY-CRATES.txt (needs: cargo install cargo-about --features cli).
set -eu
cd "$(dirname "$0")/../.."
cargo about generate -c packaging/licenses/about.toml packaging/licenses/about.hbs -o THIRD-PARTY-CRATES.txt
