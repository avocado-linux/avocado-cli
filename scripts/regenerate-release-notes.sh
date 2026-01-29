#!/bin/bash
# Regenerate release notes for all GitHub releases from commit history
# This script walks through all releases and generates concise release notes
# from the commits between each release and its predecessor.
#
# Usage: ./scripts/regenerate-release-notes.sh [--dry-run]
#
# Options:
#   --dry-run    Show what would be done without making changes

set -euo pipefail

REPO_URL="https://github.com/avocado-linux/avocado-cli"
DRY_RUN=false

if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=true
    echo "=== DRY RUN MODE ==="
    echo
fi

# Patterns for commits to skip (trivial changes)
SKIP_PATTERNS=(
    "^[0-9]+\.[0-9]+\.[0-9]+ release$"
    "^release [0-9]"
    "^bump version"
    "^version bump"
    "^Merge branch"
    "^Merge pull request"
)

# Check if a commit message should be skipped
should_skip_commit() {
    local msg="$1"
    for pattern in "${SKIP_PATTERNS[@]}"; do
        if echo "$msg" | grep -qiE "$pattern"; then
            return 0
        fi
    done
    return 1
}

# Capitalize first letter
capitalize() {
    echo "$1" | sed 's/^./\U&/'
}

# Generate release notes from commits
generate_notes() {
    local prev_tag="$1"
    local current_tag="$2"
    local notes=""
    local changes=()
    
    # Get commits between tags (first line only)
    while IFS= read -r line; do
        # Skip empty lines
        [[ -z "$line" ]] && continue
        
        # Extract commit message (remove hash prefix)
        local msg
        msg=$(echo "$line" | sed 's/^[a-f0-9]* //')
        
        # Skip trivial commits
        if should_skip_commit "$msg"; then
            continue
        fi
        
        # Capitalize and add to changes
        msg=$(capitalize "$msg")
        changes+=("- $msg")
    done < <(git log --oneline "$prev_tag..$current_tag" 2>/dev/null)
    
    # Build the notes
    if [[ ${#changes[@]} -gt 0 ]]; then
        notes="## Changes"$'\n'$'\n'
        for change in "${changes[@]}"; do
            notes+="$change"$'\n'
        done
    else
        notes="## Changes"$'\n'$'\n'"- Minor updates and improvements"$'\n'
    fi
    
    notes+=$'\n'"**Full Changelog**: $REPO_URL/compare/$prev_tag...$current_tag"
    
    echo "$notes"
}

# Get all tags sorted by version
get_sorted_tags() {
    git tag --list | grep -E '^[0-9]+\.[0-9]+\.[0-9]+' | sort -V
}

# Get all releases from GitHub
get_releases() {
    gh release list --limit 100 --json tagName -q '.[].tagName' | sort -V
}

echo "Fetching releases from GitHub..."
RELEASES=$(get_releases)
RELEASE_COUNT=$(echo "$RELEASES" | wc -l)
echo "Found $RELEASE_COUNT releases"
echo

# Build array of releases
declare -a RELEASE_ARRAY
while IFS= read -r tag; do
    RELEASE_ARRAY+=("$tag")
done <<< "$RELEASES"

# Process each release
for i in "${!RELEASE_ARRAY[@]}"; do
    CURRENT_TAG="${RELEASE_ARRAY[$i]}"
    
    # For first release, skip (no previous tag to compare)
    if [[ $i -eq 0 ]]; then
        echo "[$CURRENT_TAG] Skipping first release (no previous tag)"
        continue
    fi
    
    PREV_TAG="${RELEASE_ARRAY[$((i-1))]}"
    
    echo "[$CURRENT_TAG] Generating notes from $PREV_TAG..."
    
    # Generate the notes
    NOTES=$(generate_notes "$PREV_TAG" "$CURRENT_TAG")
    
    if $DRY_RUN; then
        echo "--- Would update with: ---"
        echo "$NOTES"
        echo "--------------------------"
        echo
    else
        # Update the release
        if gh release edit "$CURRENT_TAG" --notes "$NOTES" 2>/dev/null; then
            echo "[$CURRENT_TAG] Updated successfully"
        else
            echo "[$CURRENT_TAG] Failed to update" >&2
        fi
    fi
done

echo
echo "Done!"
