#!/usr/bin/env bash
# Build & push multi-arch images for the current branch's HEAD commit.
# CI's verify-on-PR step expects these to exist before it will pass.
#
# Path-aware fast path: when the PR's diff against `origin/development`
# (or `origin/release` when targeting that) doesn't touch any file that
# affects a given binary, we skip the rebuild and instead retag the
# latest published `:dev` image as the per-PR tags. The CI verify step
# only checks tag existence + multi-arch manifest shape — it doesn't
# care whether the image was newly-built or retagged. Tag-only ops are
# free; full builds on linux/arm64 take 15+ minutes.
#
# Prerequisites:
#   - docker + docker buildx with a multi-platform builder configured
#     (e.g., `docker buildx create --name multiarch --use --bootstrap`)
#   - logged in to ghcr.io with a PAT that has `write:packages` scope:
#     `echo $GHCR_PAT | docker login ghcr.io -u <gh-user> --password-stdin`
#
# Usage:
#   scripts/publish-pr-image.sh                # both images, path-aware
#   scripts/publish-pr-image.sh server         # only bsmcp-server, path-aware
#   scripts/publish-pr-image.sh embedder       # only bsmcp-embedder, path-aware
#   scripts/publish-pr-image.sh both --force   # force rebuild even when
#                                              # paths look unchanged
#   scripts/publish-pr-image.sh server --force # ditto, server only

set -euo pipefail

REGISTRY="ghcr.io/bees-roadhouse"
PLATFORMS="linux/amd64,linux/arm64"
REPO_URL="https://github.com/bees-roadhouse/bookstack-mcp"

cd "$(git rev-parse --show-toplevel)"

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*= *"\(.*\)"/\1/')
BRANCH=$(git rev-parse --abbrev-ref HEAD)
SHA=$(git rev-parse HEAD)
SHORT_SHA=${SHA:0:7}
SLUG=$(echo "$BRANCH" | tr '/' '-' | tr -c 'a-zA-Z0-9._-' '-')

echo "Branch:  $BRANCH"
echo "Slug:    $SLUG"
echo "Version: $VERSION"
echo "SHA:     $SHORT_SHA"
echo "Tags:    $VERSION-$SLUG-$SHORT_SHA  (immutable)"
echo "         $VERSION-$SLUG               (rolling)"
echo

if ! docker buildx version >/dev/null 2>&1; then
  echo "error: docker buildx not available" >&2
  exit 1
fi

# Resolve the merge-base ref to diff against. PRs typically target
# `development`; the release flow targets `release`. We pick whichever
# remote ref is reachable from HEAD with the shortest history — falls
# back to development when neither is local.
resolve_diff_base() {
  for candidate in origin/development origin/release; do
    if git rev-parse --verify "$candidate" >/dev/null 2>&1; then
      echo "$candidate"
      return 0
    fi
  done
  echo "origin/development"
}

DIFF_BASE=$(resolve_diff_base)

# Files that, when changed, mean we must rebuild the SERVER binary.
# bsmcp-server's transitive deps: bsmcp-common, bsmcp-db-sqlite,
# bsmcp-db-postgres, plus its own crate. Workspace + lockfile + the
# server Dockerfile + the entrypoint also force rebuild.
SERVER_PATHS=(
  "crates/bsmcp-server"
  "crates/bsmcp-common"
  "crates/bsmcp-db-sqlite"
  "crates/bsmcp-db-postgres"
  "Cargo.toml"
  "Cargo.lock"
  "docker/Dockerfile.server"
  "entrypoint.sh"
)

# Files that, when changed, mean we must rebuild the EMBEDDER binary.
# Embedder shares all the same library crates as the server (none of
# them are server-specific), but doesn't depend on bsmcp-server itself.
EMBEDDER_PATHS=(
  "crates/bsmcp-embedder"
  "crates/bsmcp-common"
  "crates/bsmcp-db-sqlite"
  "crates/bsmcp-db-postgres"
  "Cargo.toml"
  "Cargo.lock"
  "docker/Dockerfile.embedder"
  "entrypoint.sh"
)

paths_changed_since_base() {
  local -a paths=("$@")
  local count
  # `git diff --name-only $DIFF_BASE...HEAD -- $paths` returns empty
  # when no path in the list touched.
  count=$(git diff --name-only "$DIFF_BASE...HEAD" -- "${paths[@]}" 2>/dev/null | wc -l)
  [ "$count" -gt 0 ]
}

build_and_push() {
  local image=$1
  local dockerfile=$2

  local immutable="$REGISTRY/$image:$VERSION-$SLUG-$SHORT_SHA"
  local rolling="$REGISTRY/$image:$VERSION-$SLUG"

  echo "==> Building & pushing $image ($PLATFORMS)"
  docker buildx build \
    --platform "$PLATFORMS" \
    --file "$dockerfile" \
    --tag "$immutable" \
    --tag "$rolling" \
    --label "org.opencontainers.image.source=$REPO_URL" \
    --label "org.opencontainers.image.revision=$SHA" \
    --push \
    .
  echo "    pushed: $immutable"
  echo "    pushed: $rolling"
  echo
}

# Retag the existing `:dev` image as the per-PR tags. Used when no
# relevant paths changed for this binary — CI's verify check only cares
# that the per-PR tag exists with multi-arch manifest, so a tag-only
# operation is enough. `imagetools create` preserves the multi-arch
# manifest list (no rebuild, ~2s wall clock).
retag_from_dev() {
  local image=$1
  local immutable="$REGISTRY/$image:$VERSION-$SLUG-$SHORT_SHA"
  local rolling="$REGISTRY/$image:$VERSION-$SLUG"
  local src="$REGISTRY/$image:dev"

  echo "==> No relevant paths changed for $image; retagging $src"
  if ! docker buildx imagetools inspect "$src" >/dev/null 2>&1; then
    echo "    warn: $src does not exist (project may be pre-first-release);"
    echo "          falling back to a full build."
    build_and_push "$image" "docker/Dockerfile.${image#bsmcp-}"
    return
  fi
  docker buildx imagetools create --tag "$immutable" --tag "$rolling" "$src"
  echo "    tagged: $immutable  ->  $src"
  echo "    tagged: $rolling   ->  $src"
  echo
}

publish() {
  local image=$1
  local dockerfile=$2
  local -n paths_ref=$3

  if [ "$FORCE_REBUILD" = "1" ]; then
    build_and_push "$image" "$dockerfile"
  elif paths_changed_since_base "${paths_ref[@]}"; then
    build_and_push "$image" "$dockerfile"
  else
    retag_from_dev "$image"
  fi
}

# Argument parsing: first positional is target, optional `--force` flag
# can appear in any position.
target="both"
FORCE_REBUILD=0
for arg in "$@"; do
  case "$arg" in
    server|embedder|both) target="$arg" ;;
    --force)              FORCE_REBUILD=1 ;;
    *)
      echo "usage: $0 [server|embedder|both] [--force]" >&2
      exit 2
      ;;
  esac
done

case "$target" in
  server)   publish bsmcp-server  docker/Dockerfile.server   SERVER_PATHS ;;
  embedder) publish bsmcp-embedder docker/Dockerfile.embedder EMBEDDER_PATHS ;;
  both)
    publish bsmcp-server  docker/Dockerfile.server   SERVER_PATHS
    publish bsmcp-embedder docker/Dockerfile.embedder EMBEDDER_PATHS
    ;;
esac

echo "Done. Push your branch — the build-server / build-embedder verify checks should pass."
