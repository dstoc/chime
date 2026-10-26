#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: package-cli.sh <version> <source-epoch> <chime-binary> <output-dir>" >&2
  exit 2
fi

version=$1
source_epoch=$2
binary=$3
output_dir=$4

if [[ ! $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "version must be a stable semantic version" >&2
  exit 2
fi
if [[ ! $source_epoch =~ ^[0-9]+$ ]]; then
  echo "source epoch must be an integer" >&2
  exit 2
fi
if [[ ! -x $binary ]]; then
  echo "chime binary is missing or not executable: $binary" >&2
  exit 1
fi

archive="chime-v${version}-x86_64-unknown-linux-gnu.tar.gz"
mkdir -p "$output_dir"
staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
install -m 0755 "$binary" "$staging/chime"
install -m 0644 "$(dirname "$0")/../LICENSE" "$staging/LICENSE"

tar \
  --format=ustar \
  --sort=name \
  --mtime="@${source_epoch}" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  -C "$staging" \
  -cf - chime LICENSE \
  | gzip -n > "$output_dir/$archive"

(
  cd "$output_dir"
  sha256sum "$archive" > "$archive.sha256"
)
