#!/usr/bin/env bash
# Build & push multi-arch images for the current branch's HEAD commit.
# CI's verify-on-PR step expects these to exist before it will pass.
#
# Prerequisites:
#   - docker + docker buildx with a multi-platform builder configured
#     (e.g., `docker buildx create --name multiarch --use --bootstrap`)
#   - logged in to ghcr.io with a PAT that has `write:packages` scope:
#     `echo $GHCR_PAT | docker login ghcr.io -u <gh-user> --password-stdin`
#
# Usage:
#   scripts/publish-pr-image.sh                # build + push both images
#   scripts/publish-pr-image.sh server         # only bsmcp-server
#   scripts/publish-pr-image.sh embedder       # only bsmcp-embedder

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

target=${1:-both}
case "$target" in
  server)   build_and_push bsmcp-server  docker/Dockerfile.server   ;;
  embedder) build_and_push bsmcp-embedder docker/Dockerfile.embedder ;;
  both)
    build_and_push bsmcp-server  docker/Dockerfile.server
    build_and_push bsmcp-embedder docker/Dockerfile.embedder
    ;;
  *)
    echo "usage: $0 [server|embedder|both]" >&2
    exit 2
    ;;
esac

echo "Done. Push your branch — the build-server / build-embedder verify checks should pass."
