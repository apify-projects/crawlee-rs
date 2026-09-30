#!/usr/bin/env bash
# Every template has two manifests: Cargo.toml (path dependency, for running inside this repo)
# and Cargo.generated.toml (git dependency, what `cargo generate` produces). They must only
# differ in the package name and the `crawlee` dependency line.
set -euo pipefail
cd "$(dirname "$0")"
status=0
for dir in */; do
    dir=${dir%/}
    [ -f "$dir/Cargo.generated.toml" ] || continue
    strip() { grep -vE '^(name|crawlee) = |^# Points at this checkout' "$1"; }
    if ! diff <(strip "$dir/Cargo.toml") <(strip "$dir/Cargo.generated.toml") >/dev/null; then
        echo "templates/$dir: Cargo.toml and Cargo.generated.toml differ:"
        diff <(strip "$dir/Cargo.toml") <(strip "$dir/Cargo.generated.toml") || true
        status=1
    fi
done
[ "$status" -eq 0 ] && echo "template manifests are in sync"
exit "$status"
