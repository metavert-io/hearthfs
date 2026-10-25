#!/usr/bin/env bash
# Run the agent eval (agent_eval.sh) for one or more corpora: the raw
# condition, then the hearthfs condition over a fresh HearthFS mount of the
# same corpus, handling the mount lifecycle.
#
# Usage: ./scripts/eval/run_stage.sh <root> <corpus> [<corpus>...]
#
# Environment (besides agent_eval.sh's):
#   HEARTHFS_BIN         the hearthfs binary (default: target/release/hearthfs)
#   HEARTHFS_EVAL_MOUNT  scratch mountpoint (default: /tmp/hearthfs-eval-mount)
#   HEARTHFS_EVAL_PORT   NFS port for the scratch mount (default: 11177)

set -euo pipefail

ROOT="${1:?usage: run_stage.sh <root> <corpus> [<corpus>...]}"
shift

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
HEARTHFS="${HEARTHFS_BIN:-$REPO/target/release/hearthfs}"
MOUNT="${HEARTHFS_EVAL_MOUNT:-/tmp/hearthfs-eval-mount}"
PORT="${HEARTHFS_EVAL_PORT:-11177}"
LOG="${TMPDIR:-/tmp}/hearthfs-eval-mount.log"
export HEARTHFS_EVAL_MOUNT="$MOUNT"
SERVER_PID=""

cleanup() {
    # SIGTERM makes the server unmount before it exits.
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        for _ in $(seq 1 50); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.2
        done
    fi
    SERVER_PID=""
    if mount | grep -qF "$MOUNT"; then
        umount "$MOUNT" 2>/dev/null || true
    fi
}
trap cleanup EXIT

for CORPUS in "$@"; do
    echo "=== $CORPUS: raw condition"
    "$REPO/scripts/eval/agent_eval.sh" "$ROOT" "$CORPUS" raw

    echo "=== $CORPUS: hearthfs condition (mounting)"
    cleanup
    mkdir -p "$MOUNT"
    RUST_LOG=info "$HEARTHFS" mount "$ROOT/$CORPUS/data" "$MOUNT" --port "$PORT" >"$LOG" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 30); do
        mount | grep -qF "$MOUNT" && break
        sleep 1
    done
    if ! mount | grep -qF "$MOUNT"; then
        echo "error: mount failed for $CORPUS" >&2
        cat "$LOG" >&2
        exit 1
    fi
    # The index builds in the background after mount; don't let agents race
    # an empty index (2h ceiling covers a 10k-file corpus with embeddings).
    echo "--- waiting for initial indexing"
    for _ in $(seq 1 7200); do
        grep -q 'initial indexing complete' "$LOG" && break
        sleep 1
    done
    grep -q 'initial indexing complete' "$LOG" \
        || echo "warning: indexing not confirmed complete after 2h; proceeding" >&2
    "$REPO/scripts/eval/agent_eval.sh" "$ROOT" "$CORPUS" hearthfs
    cleanup
done

echo "=== stage complete"
