#!/usr/bin/env bash
#
# Re-extract lib/tracing-limit from vectordotdev/vector and fast-forward the
# `upstream` branch. See UPSTREAM.md.
#
# The --path/--path-rename arguments below determine the rewritten commit IDs.
# Changing them breaks the fast-forward property of the `upstream` branch.

set -euo pipefail

VECTOR_URL="https://github.com/vectordotdev/vector.git"
UPSTREAM_BRANCH="upstream"
WORK_DIR="${TMPDIR:-/tmp}/tracing-limit-sync"
REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

command -v git-filter-repo >/dev/null || {
    echo "git-filter-repo is required: brew install git-filter-repo" >&2
    exit 1
}

# git refuses to fetch into the currently checked-out branch.
if [ "$(git -C "$REPO_ROOT" symbolic-ref --short -q HEAD)" = "$UPSTREAM_BRANCH" ]; then
    echo "Checked out on '$UPSTREAM_BRANCH'; switch to 'wiz' first." >&2
    exit 1
fi

mkdir -p "$WORK_DIR"

# Keep a persistent Vector clone so repeat syncs only fetch new objects.
if [ -d "$WORK_DIR/vector/.git" ]; then
    echo "==> Fetching Vector"
    git -C "$WORK_DIR/vector" fetch --prune origin
    git -C "$WORK_DIR/vector" reset --hard origin/master
else
    echo "==> Cloning Vector (~150 MB)"
    git clone "$VECTOR_URL" "$WORK_DIR/vector"
fi

# filter-repo rewrites history destructively, so always work on a throwaway copy.
echo "==> Extracting lib/tracing-limit"
rm -rf "$WORK_DIR/extract"
cp -r "$WORK_DIR/vector" "$WORK_DIR/extract"
git -C "$WORK_DIR/extract" filter-repo --force \
    --path lib/tracing-limit --path lib/trace-limit \
    --path-rename lib/tracing-limit/: --path-rename lib/trace-limit/: \
    --refs refs/heads/master

echo "==> Updating $UPSTREAM_BRANCH"
git -C "$REPO_ROOT" fetch --no-tags "$WORK_DIR/extract" \
    "master:$UPSTREAM_BRANCH" || {
    echo
    echo "Fetch was rejected as a non-fast-forward. This means the extraction no" >&2
    echo "longer reproduces the existing $UPSTREAM_BRANCH history. Do not force-push;" >&2
    echo "check whether the filter arguments above were changed, or whether Vector" >&2
    echo "rewrote its own history." >&2
    exit 1
}

cat <<EOF

==> $UPSTREAM_BRANCH is now at:
$(git -C "$REPO_ROOT" log -1 --format='%h %ad %s' --date=short "$UPSTREAM_BRANCH")

Next:
  git push origin $UPSTREAM_BRANCH
  git checkout wiz && git merge $UPSTREAM_BRANCH

Conflicts in src/lib.rs and Cargo.toml are expected -- see UPSTREAM.md.
EOF
