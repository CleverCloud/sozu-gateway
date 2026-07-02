#!/usr/bin/env bash
# Shared helpers for the end-to-end scripts (e2e.sh, e2e-gateway.sh, e2e-l4.sh).
# Source this file; do not run it directly.
#
# The controller image is pushed to an ephemeral, anonymous registry (ttl.sh) by
# default, so the suite runs without registry credentials. Export IMAGE to reuse
# a prebuilt image (and skip the build) across suites.
#
# Trust note: ttl.sh is world-writable and its tags are anonymous — anyone who
# guesses a tag can overwrite it. The suite therefore deploys by *digest*
# (resolved from our own push), so the cluster can only ever pull the exact
# bytes we built. Still, prefer IMAGE=<your registry> for anything beyond a
# throwaway cluster.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE="${HELM_RELEASE:-sozu-gateway}"
NS="${HELM_NS:-sozu-system}"
DEMO_NS="${DEMO_NS:-sozu-demo}"

# Build + push the controller image unless IMAGE is already set. Exports IMAGE,
# DIGEST (when resolvable), REPO and TAG for the caller.
ensure_image() {
  if [ -z "${IMAGE:-}" ]; then
    local rand
    rand="$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' ')"
    IMAGE="ttl.sh/sozu-gw-${rand}:1h"
    echo "==> build + push controller image: $IMAGE"
    docker build -q -t "$IMAGE" "$ROOT" >/dev/null
    docker push -q "$IMAGE" >/dev/null 2>&1 || docker push "$IMAGE"
    # Resolve the digest of what WE just pushed, so the cluster pulls exactly
    # those bytes even though the ttl.sh tag itself is anonymous-writable.
    DIGEST="$(docker inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$IMAGE" \
      | grep "^${IMAGE%:*}@" | head -1 | cut -d@ -f2 || true)"
    if [ -n "${DIGEST:-}" ]; then
      echo "==> pinned by digest: $DIGEST"
    else
      echo "==> WARNING: could not resolve the pushed digest; deploying by tag"
    fi
  else
    echo "==> using prebuilt image: $IMAGE"
  fi
  export IMAGE DIGEST
  REPO="${IMAGE%:*}"
  TAG="${IMAGE##*:}"
}

# Install/upgrade the add-on. Extra `helm --set` flags are passed through, e.g.
#   ensure_addon --set l4.tcpServices.9000="sozu-demo/echo-tcp:9000"
ensure_addon() {
  echo "==> helm upgrade --install $RELEASE $*"
  helm upgrade --install "$RELEASE" "$ROOT/charts/sozu-gateway" -n "$NS" --create-namespace \
    --set image.controller.repository="$REPO" \
    --set image.controller.tag="$TAG" \
    --set image.controller.digest="${DIGEST:-}" \
    --set image.controller.pullPolicy=Always \
    "$@" --wait --timeout 180s
  kubectl rollout status deploy/"$RELEASE" -n "$NS" --timeout 120s
}

# Install the Gateway API standard-channel CRDs (idempotent).
ensure_gateway_api_crds() {
  echo "==> Gateway API CRDs (v1.2.1 standard channel)"
  kubectl apply -f \
    "https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.2.1/standard-install.yaml" >/dev/null
  kubectl wait --for=condition=Established \
    crd/httproutes.gateway.networking.k8s.io --timeout=60s >/dev/null
}

ensure_demo_ns() {
  kubectl create namespace "$DEMO_NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
}

# Port-forward to the gateway Service; args are `local:remote` port pairs. Sets
# PF_PID and installs an EXIT trap that kills it.
pf_start() {
  kubectl -n "$NS" port-forward "svc/$RELEASE" "$@" >/tmp/sozu-e2e-pf.log 2>&1 &
  PF_PID=$!
  trap 'kill "$PF_PID" 2>/dev/null || true' EXIT
  sleep 3
}

# Assert two values are equal, or fail the script.
assert_eq() {
  if [ "$1" = "$2" ]; then
    echo "  OK   $3 ($1)"
  else
    echo "  FAIL $3: expected '$2', got '$1'"
    exit 1
  fi
}
