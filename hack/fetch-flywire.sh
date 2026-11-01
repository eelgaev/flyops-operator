#!/usr/bin/env bash
# Download the pinned FlyWire v783 data into ./brain (or $1) with the committed mapping, for local runs:
#   hack/fetch-flywire.sh && FLYOPS_BRAIN_DIR=brain cargo run --release
# The container image bakes the same files at build time (see Containerfile).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$here/brain}
repo=https://raw.githubusercontent.com/philshiu/Drosophila_brain_model/91bdd1e7dcf193f3e7ca5a8933497fcef63b7960
mkdir -p "$out"
for f in Completeness_783.csv Connectivity_783.parquet; do
  [[ -f "$out/$f" ]] || curl -fSL --progress-bar -o "$out/$f" "$repo/$f"
done
(cd "$out" && if command -v sha256sum >/dev/null; then sha256sum -c "$here/data/flywire-783/SHA256SUMS"; else shasum -a 256 -c "$here/data/flywire-783/SHA256SUMS"; fi)
cp "$here/data/flywire-783/mapping.json" "$here/data/flywire-783/NOTICE" "$out/"
echo "FlyWire v783 brain ready in $out"
