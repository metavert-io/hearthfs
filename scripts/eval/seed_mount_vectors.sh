#!/usr/bin/env bash
# Seed a HearthFS mount's vector store with Voyage embeddings that the
# retrieval eval already built, so a Voyage-backed agent-eval run doesn't
# pay to embed the corpus through the API a second time.
#
# Usage: ./scripts/eval/seed_mount_vectors.sh <root> <corpus>
# Then:  HEARTHFS_EMBEDDER=voyage:voyage-4 ./scripts/eval/run_stage.sh <root> <corpus>
#
# Requires sqlite3, a retrieval-eval run with HEARTHFS_EMBEDDER=voyage:voyage-4,
# and one earlier mount of the corpus (which creates its ~/.hearthfs index dir).

set -euo pipefail

ROOT="${1:?usage: seed_mount_vectors.sh <root> <corpus>}"
CORPUS="${2:?missing corpus}"

CACHE="${HEARTHFS_EVAL_CACHE:-${TMPDIR:-/tmp}/hearthfs-eval}"
SRC="$CACHE/$CORPUS/vectors-voyage-4.db"
[ -f "$SRC" ] || { echo "error: no eval-cache voyage vectors at $SRC" >&2; exit 1; }

# The mount keys its data dir by FNV-1a hash of the source path; find it by
# matching an indexed path from the corpus in each fts.db.
DATA_DIR="$ROOT/$CORPUS/data"
PROBE=$(cd "$DATA_DIR" && find . -type f -name '*.md' | head -1 | sed 's|^\./||')
TARGET=""
for d in "$HOME/.hearthfs"/*/; do
    [ -f "$d/fts.db" ] || continue
    if sqlite3 "$d/fts.db" "SELECT 1 FROM indexed_files WHERE path='$PROBE' LIMIT 1" 2>/dev/null | grep -q 1; then
        TARGET="$d"
        break
    fi
done
[ -n "$TARGET" ] || { echo "error: no ~/.hearthfs dir contains $PROBE (mount the corpus once first)" >&2; exit 1; }

sqlite3 "$SRC" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
cp "$SRC" "$TARGET/vectors.db"
rm -f "$TARGET/vectors.db-wal" "$TARGET/vectors.db-shm"
echo "seeded $TARGET/vectors.db with $(sqlite3 "$TARGET/vectors.db" 'SELECT COUNT(*) FROM vectors') voyage vectors"
echo "model id: $(sqlite3 "$TARGET/vectors.db" "SELECT value FROM meta WHERE key='model_id'")"
