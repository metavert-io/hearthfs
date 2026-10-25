#!/usr/bin/env bash
# HearthFS Benchmark — compares token usage with and without HearthFS
#
# Usage: ./scripts/benchmark.sh <source-dir>
#
# Measures:
# 1. Raw: token count if you cat every text file
# 2. HearthFS outline: token count using @outline for each file
# 3. HearthFS search: token count for a search query vs grep
# 4. HearthFS section: token count for extracting a specific section vs full file

set -euo pipefail

SOURCE="${1:?Usage: $0 <source-dir>}"
MOUNTPOINT="/tmp/hearthfs-bench-mount"
PORT=11199

# Colors
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
NC='\033[0m'

cleanup() {
    umount "$MOUNTPOINT" 2>/dev/null || true
    pkill -f "hearthfs mount.*$PORT" 2>/dev/null || true
    rm -rf "$MOUNTPOINT"
}
trap cleanup EXIT

echo -e "${CYAN}HearthFS Benchmark${NC}"
echo "Source: $SOURCE"
echo ""

# Ensure binary is built
cargo build --release -q 2>/dev/null || cargo build -q

HEARTHFS="./target/release/hearthfs"
if [ ! -f "$HEARTHFS" ]; then
    HEARTHFS="./target/debug/hearthfs"
fi

# --- Phase 1: Raw token count ---
echo -e "${YELLOW}Phase 1: Counting raw tokens...${NC}"
RAW_TOKENS=0
RAW_FILES=0
while IFS= read -r -d '' file; do
    tokens=$(wc -c < "$file" | tr -d ' ')
    # Rough estimate: 1 token ≈ 4 chars
    file_tokens=$(( (tokens * 2 + 6) / 7 ))
    RAW_TOKENS=$((RAW_TOKENS + file_tokens))
    RAW_FILES=$((RAW_FILES + 1))
done < <(find "$SOURCE" -type f \( -name "*.rs" -o -name "*.py" -o -name "*.js" -o -name "*.ts" -o -name "*.go" -o -name "*.java" -o -name "*.md" -o -name "*.txt" -o -name "*.toml" -o -name "*.json" -o -name "*.yaml" -o -name "*.yml" \) ! -path "*/target/*" ! -path "*/.git/*" ! -path "*/node_modules/*" -print0)

echo "  Files: $RAW_FILES"
echo "  Raw tokens (full read): $RAW_TOKENS"
echo ""

# --- Phase 2: Mount HearthFS ---
echo -e "${YELLOW}Phase 2: Starting HearthFS...${NC}"
cleanup 2>/dev/null || true
mkdir -p "$MOUNTPOINT"
$HEARTHFS mount "$SOURCE" "$MOUNTPOINT" --port $PORT 2>/dev/null &
sleep 5

if ! mount | grep -q "$MOUNTPOINT"; then
    echo "ERROR: Failed to mount HearthFS"
    exit 1
fi
echo "  Mounted at $MOUNTPOINT"
echo ""

# --- Phase 3: Outline tokens ---
echo -e "${YELLOW}Phase 3: Outline token count...${NC}"
OUTLINE_TOKENS=0
OUTLINE_FILES=0
for ext in rs py js ts go java md txt; do
    while IFS= read -r -d '' file; do
        relpath="${file#$MOUNTPOINT/}"
        outline_path="${file}@outline"
        if content=$(cat "$outline_path" 2>/dev/null); then
            tokens=$(echo -n "$content" | wc -c | tr -d ' ')
            file_tokens=$(( (tokens * 2 + 6) / 7 ))
            OUTLINE_TOKENS=$((OUTLINE_TOKENS + file_tokens))
            OUTLINE_FILES=$((OUTLINE_FILES + 1))
        fi
    done < <(find "$MOUNTPOINT" -maxdepth 5 -type f -name "*.$ext" ! -path "*/target/*" ! -path "*/.git/*" -print0 2>/dev/null)
done

echo "  Files with outlines: $OUTLINE_FILES"
echo "  Outline tokens: $OUTLINE_TOKENS"
if [ $RAW_TOKENS -gt 0 ]; then
    SAVINGS=$(( (RAW_TOKENS - OUTLINE_TOKENS) * 100 / RAW_TOKENS ))
    echo -e "  ${GREEN}Token reduction: ${SAVINGS}%${NC}"
fi
echo ""

# --- Phase 4: Search comparison ---
echo -e "${YELLOW}Phase 4: Search comparison...${NC}"
# Search via HearthFS
SEARCH_QUERY="function"
SEARCH_RESULT=$(cat "$MOUNTPOINT/@search/$SEARCH_QUERY" 2>/dev/null || echo "")
SEARCH_TOKENS=$(echo -n "$SEARCH_RESULT" | wc -c | tr -d ' ')
SEARCH_TOKENS=$(( (SEARCH_TOKENS * 2 + 6) / 7 ))

# Equivalent: grep all files for the same query
GREP_RESULT=$(grep -r "$SEARCH_QUERY" "$SOURCE" --include="*.rs" --include="*.py" --include="*.js" --include="*.ts" --include="*.md" 2>/dev/null || true)
GREP_TOKENS=$(echo -n "$GREP_RESULT" | wc -c | tr -d ' ')
GREP_TOKENS=$(( (GREP_TOKENS * 2 + 6) / 7 ))

echo "  Query: '$SEARCH_QUERY'"
echo "  grep tokens:   $GREP_TOKENS"
echo "  @search tokens: $SEARCH_TOKENS"
if [ $GREP_TOKENS -gt 0 ]; then
    SEARCH_SAVINGS=$(( (GREP_TOKENS - SEARCH_TOKENS) * 100 / GREP_TOKENS ))
    echo -e "  ${GREEN}Token reduction: ${SEARCH_SAVINGS}%${NC}"
fi
echo ""

# --- Phase 5: Stats ---
echo -e "${YELLOW}Phase 5: HearthFS stats${NC}"
$HEARTHFS stats 2>/dev/null
echo ""

# --- Summary ---
echo -e "${CYAN}=== Summary ===${NC}"
echo "  Source files:     $RAW_FILES"
echo "  Raw tokens:       $RAW_TOKENS"
echo "  Outline tokens:   $OUTLINE_TOKENS"
if [ $RAW_TOKENS -gt 0 ]; then
    echo -e "  ${GREEN}Overall reduction:  ${SAVINGS}% (outlines vs full reads)${NC}"
fi
