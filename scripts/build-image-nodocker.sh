#!/usr/bin/env bash
# Build and push the controller image WITHOUT docker.
#
# The e2e suites call `docker build` (scripts/e2e-lib.sh, `ensure_image`), which
# the devcontainer cannot do — it has no docker daemon. This appends the freshly
# built release binary as a layer onto the same base the Dockerfile uses, with
# the same path, entrypoint and uid, so what runs on the cluster is what the
# Dockerfile would have produced.
#
#   cargo build --release -p sozu-gw-controller
#   eval "$(bash scripts/build-image-nodocker.sh | tail -2)"   # exports IMAGE + DIGEST
#   bash scripts/e2e.sh                                        # ensure_image skips docker
#
# Needs `crane`: go install github.com/google/go-containerregistry/cmd/crane@latest
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
BIN="$ROOT/target/release/sozu-gw-controller"

[ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }

mkdir -p "$WORK/usr/local/bin"
cp "$BIN" "$WORK/usr/local/bin/sozu-gw-controller"
chmod 0755 "$WORK/usr/local/bin/sozu-gw-controller"
tar -C "$WORK" -cf "$WORK/layer.tar" usr

RAND="$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' ')"
IMAGE="ttl.sh/sozu-gw-${RAND}:1h"

echo "==> crane append -> $IMAGE"
crane append --platform linux/amd64 -b debian:bookworm-slim -f "$WORK/layer.tar" -t "$IMAGE" >/dev/null
echo "==> crane mutate (entrypoint + uid 1000)"
crane mutate "$IMAGE" \
  --entrypoint /usr/local/bin/sozu-gw-controller \
  --user 1000:1000 \
  -t "$IMAGE" >/dev/null

DIGEST="$(crane digest "$IMAGE")"
echo "IMAGE=$IMAGE"
echo "DIGEST=$DIGEST"
rm -rf "$WORK"
