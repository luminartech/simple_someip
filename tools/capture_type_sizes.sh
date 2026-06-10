#!/usr/bin/env bash
# Capture -Zprint-type-sizes async-future layouts (PR 0, issue #125).
#
#   Host capture  — compiles the bare_metal_e2e test (instantiates
#                   Client+Server with mock deps) on the host triple.
#   Thumb capture — compiles tools/size_probe (instantiates the
#                   client futures no_std) for thumbv7em-none-eabihf.
#                   THESE are the authoritative numbers.
#
# Dedicated CARGO_TARGET_DIRs force fresh builds: rustc only emits
# type sizes for crates it actually (re)compiles.
#
# Usage: tools/capture_type_sizes.sh [out_dir]   (default target/type-sizes)
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-target/type-sizes}"
mkdir -p "$OUT"
# Resolve to an absolute path so the thumb capture's `cd` into
# tools/size_probe can't break a relative CARGO_TARGET_DIR/out path.
OUT="$(cd "$OUT" && pwd)"
# Wipe previous capture builds — a warm CARGO_TARGET_DIR turns the
# build into a no-op and rustc emits no type sizes on a no-op.
rm -rf "$OUT/host" "$OUT/thumb"

echo "== host capture (bare_metal_e2e test) =="
RUSTFLAGS="-Zprint-type-sizes" CARGO_TARGET_DIR="$OUT/host" \
  cargo +nightly test --no-run --features client,server,bare_metal \
  --test bare_metal_e2e >"$OUT/host_raw.txt" 2>&1 \
  || { echo "host capture FAILED; tail:"; tail -20 "$OUT/host_raw.txt"; exit 1; }

echo "== thumb capture (size_probe) =="
( cd tools/size_probe && \
  RUSTFLAGS="-Zprint-type-sizes" CARGO_TARGET_DIR="$OUT/thumb" \
  cargo +nightly build --release --target thumbv7em-none-eabihf \
  >"$OUT/thumb_raw.txt" 2>&1 ) \
  || { echo "thumb capture FAILED; tail:"; tail -20 "$OUT/thumb_raw.txt"; exit 1; }

summarize() {
  # `|| true` is load-bearing twice over: grep exits 1 on an empty
  # capture (no async-future lines), and `head -40` SIGPIPEs `sort`
  # (exit 141) whenever there are >40 rows — either would abort the
  # whole script under `set -euo pipefail` mid-summary.
  grep 'print-type-size type' "$1" \
    | grep -E 'async fn body|async block' \
    | sed -E 's/.*type: `([^`]+)`: ([0-9]+) bytes.*/\2 \1/' \
    | sort -rn | head -40 \
    | awk '{printf "| %s | %s |\n", $1, substr($0, length($1)+2)}' \
    || true
}

{
  echo "# Type-size capture — $(rustc +nightly --version)"
  echo
  echo "## thumbv7em (authoritative)"
  echo "| bytes | future |"
  echo "|---|---|"
  summarize "$OUT/thumb_raw.txt"
  echo
  echo "## host x86_64 (proxy)"
  echo "| bytes | future |"
  echo "|---|---|"
  summarize "$OUT/host_raw.txt"
} >"$OUT/summary.md"

echo "wrote $OUT/summary.md"
