#!/usr/bin/env bash
set -euo pipefail
# Disk hygiene guard for convex-backend - prevents 100GiB target bloat.
# Auto-cleans when free <20GiB or target >15GiB. Safe to run anytime.
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "=== disk hygiene check ==="
df -h / | head -n 2
echo ""
if [ -d target ]; then
  du -sh target 2>/dev/null || true
  du -sh target/debug 2>/dev/null | head -n 1 || true
else
  echo "no target/ dir"
fi
echo ""

FREE_GB=$(df -g / | awk 'NR==2{print $4}' | tr -d 'Gi')
TARGET_GB=$(du -sg target 2>/dev/null | cut -f1 || echo 0)
echo "free: ${FREE_GB}Gi  target: ${TARGET_GB}Gi"

NEEDS_CLEAN=0
if [ "$FREE_GB" -lt 20 ]; then NEEDS_CLEAN=1; echo "-> low free space (<20Gi), will clean"; fi
if [ "$TARGET_GB" -gt 15 ]; then NEEDS_CLEAN=1; echo "-> target bloated (>15Gi), will clean"; fi
# also allow forced: ./scripts/disk-hygiene.sh --force
if [ "${1:-}" = "--force" ] || [ "${1:-}" = "--clean" ]; then NEEDS_CLEAN=1; fi

if [ "$NEEDS_CLEAN" -eq 0 ]; then
  echo "ok - no clean needed"
  exit 0
fi

echo "--- cleaning ---"
# 1. Remove incremental cruft (safest, biggest win)
if [ -d target/debug/incremental ]; then
  SZ=$(du -sh target/debug/incremental 2>/dev/null | cut -f1 || echo "?")
  echo "removing target/debug/incremental ($SZ)..."
  rm -rf target/debug/incremental
fi
if [ -d target/debug/deps ]; then
  # keep but prune old .d files older than 7d if requested
  echo "target/debug/deps exists - leaving (use --force-deep to prune)"
fi
# 2. Prune cargo cache not needed? keep
# 3. If still bloated, offer deeper clean
if [ "${1:-}" = "--force-deep" ]; then
  echo "deep clean: cargo clean - may free 10-20Gi"
  cargo clean 2>&1 | tail -n 5 || true
  df -h / | head -n 2
  du -sh target 2>/dev/null || echo "target removed"
fi
echo "--- after ---"
df -h / | head -n 2
du -sh target 2>/dev/null || echo "no target"
echo "done"
