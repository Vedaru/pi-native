#!/usr/bin/env bash
# Build a release binary and pack it into a distributable tarball.
#
#   scripts/package.sh                 # host target
#   scripts/package.sh <target-triple> # cross/static target (e.g. *-musl)
#
# Output: dist/pipelets-<version>-<target>.tar.gz (+ .sha256)
set -euo pipefail
cd "$(dirname "$0")/.."

target="${1:-}"
if [[ -n "$target" ]]; then
    cargo build --release --target "$target"
    out_dir="target/$target/release"
    suffix="$target"
else
    cargo build --release
    out_dir="target/release"
    suffix="$(rustc -vV | sed -n 's/^host: //p')"
fi

version="$(cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="pipelets"))')"

name="pipelets-${version}-${suffix}"
dist="dist"
rm -rf "$dist/$name"
mkdir -p "$dist/$name"
cp "$out_dir/pipelets" "$dist/$name/"
cp README.md LICENSE "$dist/$name/"

tar -C "$dist" -czf "$dist/$name.tar.gz" "$name"
( cd "$dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )

echo "packaged dist/$name.tar.gz"
"$dist/$name/pipelets" --version
