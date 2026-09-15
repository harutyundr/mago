#!/usr/bin/env bash
# Rebase the type-inference patch branch onto the latest upstream, test it,
# and (with --tag) publish: pushing the tag triggers the Publish workflow,
# which builds release binaries and pushes the multi-arch image to the
# devbox GitLab registry. See PUBLISHING.md.
#
# Usage:
#   scripts/update-upstream.sh                # fetch + rebase + test
#   scripts/update-upstream.sh --tag          # also push branch + tag (publishes)
#   scripts/update-upstream.sh --skip-tests   # skip cargo test
#   scripts/update-upstream.sh --regen-diff   # also regenerate ~/devbox/patches/type-inferrence.diff
#   scripts/update-upstream.sh --dry-run      # print what would run, change nothing
#
# On rebase conflicts the script stops, you resolve them, then re-run —
# the rebase is not automatic because upstream regularly touches the
# analyzer files the patch also touches.

set -euo pipefail

BRANCH="feature/type-inference-enhancement"
UPSTREAM_REMOTE="upstream"
UPSTREAM_REF="upstream/main"
FORK_REMOTE="origin"
PATCH_OUT="${HOME}/devbox/patches/type-inferrence.diff"

TAG=false
RUN_TESTS=true
REGEN_DIFF=false
DRY_RUN=false

for arg in "$@"; do
    case $arg in
        --tag) TAG=true ;;
        --skip-tests) RUN_TESTS=false ;;
        --regen-diff) REGEN_DIFF=true ;;
        --dry-run) DRY_RUN=true ;;
        *)
            echo "Unknown option: $arg"
            sed -n '2,14p' "$0"
            exit 1
            ;;
    esac
done

run() {
    if [ "$DRY_RUN" = true ]; then
        echo "[dry-run] $*"
    else
        "$@"
    fi
}

cd "$(git rev-parse --show-toplevel)"

CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$CURRENT_BRANCH" != "$BRANCH" ]; then
    echo "Error: must be on $BRANCH (currently on $CURRENT_BRANCH)"
    exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "Error: worktree has uncommitted changes; commit or stash them first."
    exit 1
fi

if ! git remote get-url "$UPSTREAM_REMOTE" >/dev/null 2>&1; then
    echo "Error: remote '$UPSTREAM_REMOTE' not found."
    echo "Add it with: git remote add $UPSTREAM_REMOTE git@github.com:carthage-software/mago.git"
    exit 1
fi

echo "--- Fetching $UPSTREAM_REMOTE..."
run git fetch "$UPSTREAM_REMOTE"

BEHIND=$(git rev-list --count "HEAD..$UPSTREAM_REF")
AHEAD=$(git rev-list --count "$UPSTREAM_REF..HEAD")
echo "Patch branch is ${AHEAD} commit(s) ahead, ${BEHIND} commit(s) behind $UPSTREAM_REF."

if [ "$BEHIND" -eq 0 ]; then
    echo "Already up to date with $UPSTREAM_REF."
else
    echo "--- Rebasing onto $UPSTREAM_REF..."
    if [ "$DRY_RUN" = true ]; then
        echo "[dry-run] git rebase $UPSTREAM_REF"
    elif ! git rebase "$UPSTREAM_REF"; then
        echo
        echo "Rebase conflicts. For each conflicted file:"
        echo "  1. fix the code,  2. git add <file>,  3. git rebase --continue"
        echo "Or run 'git rebase --abort' to give up."
        echo "After the rebase completes, re-run this script."
        exit 2
    fi
fi

if [ "$RUN_TESTS" = true ]; then
    echo "--- Running tests (cargo test --workspace --locked --all-targets)..."
    run cargo test --workspace --locked --all-targets
fi

if [ "$REGEN_DIFF" = true ]; then
    echo "--- Regenerating $PATCH_OUT..."
    # Three-dot diff: patch content relative to the merge base, so it also
    # works when the branch has not just been rebased.
    run git diff "$UPSTREAM_REMOTE/main...$BRANCH" -- ':(exclude)AGENTS.md' > "$PATCH_OUT"
    echo "Wrote $(wc -l < "$PATCH_OUT") lines to $PATCH_OUT"
fi

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
SHORT_SHA=$(git rev-parse --short HEAD)
TAG_NAME="v${VERSION}-${SHORT_SHA}"

if [ "$TAG" = true ]; then
    echo "--- Pushing $BRANCH to $FORK_REMOTE..."
    run git push "$FORK_REMOTE" "$BRANCH"

    if git rev-parse -q --verify "refs/tags/$TAG_NAME" >/dev/null; then
        echo "Error: tag $TAG_NAME already exists (same HEAD pushed before?)."
        echo "Delete it first with: git tag -d $TAG_NAME"
        exit 1
    fi
    echo "--- Creating and pushing tag $TAG_NAME (triggers Publish workflow)..."
    run git tag "$TAG_NAME"
    run git push "$FORK_REMOTE" "$TAG_NAME"
    echo
    echo "Tag pushed. Monitor: gh run watch -R harutyundr/mago \$(gh run list -R harutyundr/mago -L 1 --json databaseId -q '.[0].databaseId')"
else
    echo
    echo "Done. To publish, re-run with --tag (creates $TAG_NAME)."
fi
