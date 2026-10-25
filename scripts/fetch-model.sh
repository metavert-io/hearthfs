#!/usr/bin/env bash
# Download the local embedding model HearthFS uses for semantic search:
# sentence-transformers/all-MiniLM-L6-v2 (Apache-2.0), from Hugging Face at
# a pinned revision, with SHA-256 checks.
#
# Usage: ./scripts/fetch-model.sh [dest-dir]
#   dest-dir defaults to ~/.hearthfs/models, which the CLI checks after
#   ./models. Use ./models for a repo-local copy (git ignores it).
#
# Writes <dest>/all-MiniLM-L6-v2.onnx (Hugging Face's onnx/model.onnx; the
# file name is part of the embedder's model ID, so keep it) and
# <dest>/tokenizer.json. Re-running is cheap: files that already match their
# hash are kept. The model is Apache-2.0 licensed; see the model card at
# https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2 and
# THIRD-PARTY-NOTICES.md.

set -euo pipefail

REPO="sentence-transformers/all-MiniLM-L6-v2"
REVISION="1110a243fdf4706b3f48f1d95db1a4f5529b4d41"
BASE="https://huggingface.co/$REPO/resolve/$REVISION"

DEST="${1:-$HOME/.hearthfs/models}"
mkdir -p "$DEST"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# fetch <path in the model repo> <local name> <expected sha256>
fetch() {
    local remote="$1" name="$2" want="$3"
    local target="$DEST/$name"
    if [ -f "$target" ] && [ "$(sha256 "$target")" = "$want" ]; then
        echo "ok      $target (already present)"
        return
    fi
    local tmp="$target.part"
    echo "fetch   $REPO@${REVISION:0:12} $remote"
    curl -fL --retry 3 --progress-bar -o "$tmp" "$BASE/$remote"
    local got
    got="$(sha256 "$tmp")"
    if [ "$got" != "$want" ]; then
        rm -f "$tmp"
        echo "error: SHA-256 mismatch for $remote" >&2
        echo "       expected $want" >&2
        echo "       got      $got" >&2
        exit 1
    fi
    mv "$tmp" "$target"
    echo "ok      $target"
}

fetch "onnx/model.onnx" "all-MiniLM-L6-v2.onnx" \
    "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452"
fetch "tokenizer.json" "tokenizer.json" \
    "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037"

echo "Model ready in $DEST (all-MiniLM-L6-v2, Apache-2.0)."
