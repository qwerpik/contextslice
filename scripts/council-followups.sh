#!/usr/bin/env bash
# council-followups.sh — automate the council verdict's owner-side checks.
#
# What it does:
#   1. Runs the quality gates (fmt, clippy -D warnings, workspace tests).
#   2. Asserts the linkage guard: cs-select must EXECUTE tests (count > 0),
#      so gates can never pass vacuously on an unwired crate again.
#   3. Scale probe: builds a synthetic Go corpus (default 2000 files),
#      indexes it, and reports rows vs the 1M snapshot bound with headroom.
#   4. Prints the remaining judgment calls (need a human ruling, not a run).
#
# Usage: ./scripts/council-followups.sh [scale_files]
# Env:   SKIP_FULL_GATES=1  (only cs-select/cs-index gates, faster)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SCALE="${1:-2000}"

echo "=== 1. quality gates ==="
cargo fmt --all --check && echo "fmt: OK"
if [ -n "${SKIP_FULL_GATES:-}" ]; then
    cargo clippy -p cs-select -p cs-index --all-targets --all-features -- -D warnings
else
    cargo clippy --workspace --all-targets --all-features -- -D warnings
fi
echo "clippy: OK"

echo "=== 2. linkage guard (tests must EXECUTE) ==="
OUT="$(cargo test -p cs-select 2>&1)"
echo "$OUT" | grep -E "test result" | head -n 3
COUNT="$(echo "$OUT" | grep -oE '[0-9]+ passed' | head -n1 | grep -oE '[0-9]+')"
# 14 task + 5 tuning + 3 linkage; bump this floor when stages land.
if [ "${COUNT:-0}" -lt 22 ]; then
    echo "FAIL: cs-select executed only ${COUNT:-0} tests (want >= 22) — unwired crate?"
    exit 1
fi
echo "linkage: OK ($COUNT tests executed)"

echo "=== 3. scale probe ($SCALE synthetic Go files) ==="
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/repo"
i=0
while [ "$i" -lt "$SCALE" ]; do
    d=$((i % 50))
    mkdir -p "$WORK/repo/pkg$d"
    cat > "$WORK/repo/pkg$d/file$i.go" <<EOF
package pkg$d

import "fmt"

func Handler$i(name string) string {
    return fmt.Sprintf("item %d: %s", $i, name)
}

func helper$i(x int) int { return x + $i }
EOF
    i=$((i + 1))
done
BIN="$WORK/bin"; mkdir -p "$BIN"
cargo build -q -p cs-cli
cp target/debug/contextslice "$BIN/contextslice"
CSCLI="$BIN/contextslice"
START="$(date +%s%N)"
"$CSCLI" --index "$WORK/index.db" index "$WORK/repo" >/dev/null 2>&1 || {
    echo "index failed; run: $CSCLI index --help"
    exit 1
}
END="$(date +%s%N)"
MS="$(( (END - START) / 1000000 ))"
DBSIZE="$(du -h "$WORK/index.db" | cut -f1)"
python3 - "$WORK/index.db" <<'EOF'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
rows = {}
for t in ("files", "symbols", "edges"):
    try:
        rows[t] = db.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0]
    except Exception as e:
        rows[t] = f"ERR {e}"
print("rows:", rows)
total = sum(v for v in rows.values() if isinstance(v, int))
print(f"total={total} bound=1000000 headroom={1000000 // max(total, 1)}x")
EOF
echo "index_time=${MS}ms db_size=$DBSIZE (for $SCALE files)"

echo "=== 4. judgment calls (human rulings, with pointers) ==="
cat <<'EOF'
[ ] NFKC gap: task.rs documents to_lowercase-only normalization (no
    unicode-normalization per ADR-011). Ruling: accept the documented gap,
    or schedule the crate exception. Evidence: crates/cs-select/src/task.rs
    module docs + worked-trace tests.
[ ] signature/doc in snapshot: SnapshotSymbol carries signature/doc strings
    re-exposed from the index. Ruling: confirm this does not violate the
    "no source text stored" contract (spans+signatures, never file
    contents), or scope the loader down. Evidence:
    crates/cs-index/src/selection_snapshot.rs (SnapshotSymbol).
[ ] GitHub Actions: Settings -> Actions -> General -> Allow actions, then
    re-add the CI badge to README.md (it 404s while Actions is disabled).
[ ] Re-run this script after the seed stage lands; widen the scale probe
    toward 100k files before relying on the 1M bound.
EOF
echo "ALL AUTOMATED CHECKS PASSED"
