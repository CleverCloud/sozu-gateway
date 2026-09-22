#!/usr/bin/env bash
# Shared helpers for the end-to-end scripts (e2e.sh, e2e-gateway.sh,
# e2e-l4-routes.sh).
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

# Cleanup hooks, run on exit in reverse registration order, each handed the
# script's exit status. One registry rather than one `trap ... EXIT` per
# caller: a second `trap` silently replaces the first, so a suite installing
# its own would leak the port-forward `pf_start` started (or the reverse).
E2E_EXIT_HOOKS=()
on_exit() {
  E2E_EXIT_HOOKS+=("$1")
}
run_exit_hooks() {
  local rc=$? i
  for ((i = ${#E2E_EXIT_HOOKS[@]} - 1; i >= 0; i--)); do
    "${E2E_EXIT_HOOKS[$i]}" "$rc" || true
  done
}
trap run_exit_hooks EXIT

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
#   ensure_addon --set-json 'exposure=[...]' 
ensure_addon() {
  echo "==> helm upgrade --install $RELEASE $*"
  # One replica, deliberately. The chart's default spreads hard across nodes, so
  # at the default the suites would need as many nodes as replicas and would sit
  # in `--wait` on anything smaller. These suites exercise routing, not the
  # replica topology; override REPLICAS to test the shipped default on a cluster
  # big enough for it.
  helm upgrade --install "$RELEASE" "$ROOT/charts/sozu-gateway" -n "$NS" --create-namespace \
    --set replicaCount="${REPLICAS:-1}" \
    --set image.controller.repository="$REPO" \
    --set image.controller.tag="$TAG" \
    --set image.controller.digest="${DIGEST:-}" \
    --set image.controller.pullPolicy=Always \
    "$@" --wait --timeout 180s
  kubectl rollout status deploy/"$RELEASE" -n "$NS" --timeout 120s
}

# Install the Gateway API standard-channel CRDs (idempotent).
#
# v1.6.1 is the version the generated types in crates/gateway-api are built
# from, and — since v1.6 — the version whose *standard* channel ships
# tcproutes/udproutes/tlsroutes, which the layer-4 suites need. Override
# GWAPI_VERSION to check the controller against an older bundle.
GWAPI_VERSION="${GWAPI_VERSION:-v1.6.1}"

ensure_gateway_api_crds() {
  echo "==> Gateway API CRDs ($GWAPI_VERSION standard channel)"
  kubectl apply -f \
    "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GWAPI_VERSION}/standard-install.yaml" >/dev/null
  # On the very first install `kubectl wait` can race the apiserver: it errors
  # out on a still-nil .status.conditions instead of waiting. Retry briefly.
  for _ in 1 2 3 4 5; do
    kubectl wait --for=condition=Established \
      crd/httproutes.gateway.networking.k8s.io --timeout=60s >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "FAIL: Gateway API CRDs never became Established" >&2
  return 1
}

ensure_demo_ns() {
  kubectl create namespace "$DEMO_NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
}

# Port-forward to the gateway; args are `local:remote` port pairs. Sets PF_PID
# and registers an exit hook that kills it. Returns once every pair is
# listening locally (kubectl reports each as "Forwarding from ..."), or fails
# with the port-forward's own output when it dies first, e.g. on a local port
# another suite still holds.
#
# Targets a *Ready* pod explicitly instead of `svc/`: right after a rolling
# update, `kubectl port-forward svc/...` can attach to a Terminating or
# not-yet-ready pod (it picks the first selector match, ignoring readiness),
# which makes the suite probe a proxy that is already being torn down.
pf_start() {
  local pod pair local_port svc_port target deadline
  local pairs=()
  # A fresh log per invocation: a shared path could hand `pf_listening` a
  # previous or concurrent run's "Forwarding from" line and pass before this
  # port-forward has actually bound.
  PF_LOG="$(mktemp "${TMPDIR:-/tmp}/sozu-e2e-pf.XXXXXX.log")"
  on_exit pf_log_cleanup
  pod=$(kubectl -n "$NS" get pods \
    -l "app.kubernetes.io/instance=$RELEASE" \
    -o jsonpath='{range .items[*]}{.metadata.name} {.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' \
    | awk '$2 == "True" { print $1; exit }')
  if [ -z "$pod" ]; then
    echo "FAIL: no Ready gateway pod to port-forward to" >&2
    exit 1
  fi
  # The callers speak in *Service* ports; forwarding to a pod bypasses the
  # Service's port mapping, so resolve each port through the Service's
  # targetPort (and, when that is a name, through the pod's container ports).
  for pair in "$@"; do
    local_port="${pair%%:*}"
    svc_port="${pair##*:}"
    target=$(kubectl -n "$NS" get svc "$RELEASE" \
      -o jsonpath="{.spec.ports[?(@.port==$svc_port)].targetPort}")
    if ! [[ "$target" =~ ^[0-9]+$ ]]; then
      target=$(kubectl -n "$NS" get pod "$pod" \
        -o jsonpath="{.spec.containers[*].ports[?(@.name==\"$target\")].containerPort}")
    fi
    if [ -z "$target" ]; then
      echo "FAIL: could not resolve Service port $svc_port to a container port" >&2
      exit 1
    fi
    pairs+=("${local_port}:${target}")
  done
  kubectl -n "$NS" port-forward "pod/$pod" "${pairs[@]}" >"$PF_LOG" 2>&1 &
  PF_PID=$!
  on_exit pf_stop
  deadline=$((SECONDS + 30))
  until pf_listening "${pairs[@]}"; do
    if ! kill -0 "$PF_PID" 2>/dev/null || [ "$SECONDS" -ge "$deadline" ]; then
      echo "FAIL: port-forward to $pod (${pairs[*]}) did not come up:" >&2
      sed 's/^/  /' "$PF_LOG" >&2
      exit 1
    fi
    sleep 1
  done
}

# True once the port-forward reports every local port bound. Checked per port
# rather than by counting lines: kubectl binds the ports one at a time and
# prints one line per address (127.0.0.1 and ::1), so a count reaches the
# number of pairs while the last port is still unbound.
pf_listening() {
  local pair
  for pair in "$@"; do
    grep -q "^Forwarding from .*:${pair%%:*} -> " "$PF_LOG" 2>/dev/null || return 1
  done
}

pf_stop() {
  if [ -n "${PF_PID:-}" ]; then
    kill "$PF_PID" 2>/dev/null || true
    wait "$PF_PID" 2>/dev/null || true
  fi
}

pf_log_cleanup() {
  [ -n "${PF_LOG:-}" ] && rm -f "$PF_LOG"
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

# Assert a file has a line matching the (case-insensitive, extended) regex, or
# fail the script with the file's content.
assert_grep() {
  if grep -qiE "$1" "$2"; then
    echo "  OK   $3"
  else
    echo "  FAIL $3: no line matching '$1' in $2:"
    sed 's/^/       /' "$2"
    exit 1
  fi
}

# Poll a probe until it prints the expected value, then report OK; fail with
# the last observed value once E2E_WAIT_SECS (default 60) have elapsed. The
# probe is `"$@"` and must always succeed: it prints an observation, and a
# transport error is an observation too (see `http_code` in e2e.sh).
#
# The controller debounces and applies a change within a couple of seconds;
# a fixed sleep either outlasts that by a wide margin or, under load, not at
# all. Polling makes the suite as fast as the reconcile and only ever slow
# when it is about to fail.
wait_for() {
  local want="$1" label="$2" deadline got
  shift 2
  deadline=$((SECONDS + ${E2E_WAIT_SECS:-60}))
  while :; do
    got="$("$@")" || got="<probe failed>"
    if [ "$got" = "$want" ]; then
      echo "  OK   $label ($got)"
      return 0
    fi
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "  FAIL $label: expected '$want', last observed '$got' after ${E2E_WAIT_SECS:-60}s"
      exit 1
    fi
    sleep 1
  done
}
