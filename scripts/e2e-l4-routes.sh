#!/usr/bin/env bash
# End-to-end test for layer-4 routing through the Gateway API: a TCPRoute and a
# UDPRoute attached to TCP/UDP Gateway listeners, forwarded by Sōzu to echo
# backends, probed by an in-cluster socat client (see `l4_probe`).
#
# This is the supported replacement for the l4.tcpServices/udpServices
# ConfigMaps (scripts/e2e-l4.sh still covers those while they exist).
#
# It also exercises the conflict path: a second TCPRoute claiming the same port
# must lose, say so on its own status, and — the part that matters — leave every
# other route serving.
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

TCP_PORT=9000
UDP_PORT=9001

# Send one line to the gateway Service and echo the reply.
#
# An in-cluster client rather than `kubectl port-forward`: port-forward is
# TCP-only, so a UDPRoute cannot be exercised through it at all. Talking to the
# Service also covers the piece the exposure table exists for — the Service port
# mapping onto Sōzu's in-pod bind — which a port-forward to the pod bypasses.
l4_probe() {
  local proto="$1" port="$2" payload="$3" target
  target="socat -T5 - $( [ "$proto" = udp ] && echo UDP-SENDTO || echo TCP ):${RELEASE}.${NS}:${port}"
  printf '%s\n' "$payload" | kubectl -n "$DEMO_NS" run "probe-${proto}-$RANDOM" \
    --rm -i --quiet --restart=Never --image=alpine/socat:1.8.0.0 \
    --command -- sh -c "$target" 2>/dev/null | tr -d '\r'
}

# Release the ports this suite claims, pass or fail. The suites share the demo
# namespace and run in sequence, and this one is the only one that *claims a
# socket*: leaving its TCPRoute behind makes the next suite's tcp-services
# mapping lose port 9000 to it — correctly, and confusingly.
#
# Only the routing objects. The echo Deployments and Services are shared demo
# fixtures other suites also apply, and deleting them here would race their
# recreation.
cleanup() {
  kubectl -n "$DEMO_NS" delete --ignore-not-found \
    tcproute/echo-tcp tcproute/echo-tcp-intruder udproute/echo-udp gateway/l4 \
    >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> context: $(kubectl config current-context)"
ensure_image
ensure_gateway_api_crds

# Only Helm can open a port on the Service, so the layer-4 ports are declared in
# the exposure table. The whole list is restated: --set on a list replaces it.
ensure_addon --set-json "exposure=[
  {\"name\":\"http\",\"port\":80,\"bind\":8080,\"protocol\":\"HTTP\",\"transport\":\"TCP\"},
  {\"name\":\"https\",\"port\":443,\"bind\":8443,\"protocol\":\"HTTPS\",\"transport\":\"TCP\"},
  {\"name\":\"echo-tcp\",\"port\":${TCP_PORT},\"bind\":${TCP_PORT},\"protocol\":\"TCP\",\"transport\":\"TCP\"},
  {\"name\":\"echo-udp\",\"port\":${UDP_PORT},\"bind\":${UDP_PORT},\"protocol\":\"UDP\",\"transport\":\"UDP\"}]"
ensure_demo_ns

echo "==> apply the Gateway API layer-4 example"
kubectl apply -f "$ROOT/examples/api-gateway/l4-routes.yaml" >/dev/null
kubectl rollout status deploy/echo-tcp -n "$DEMO_NS" --timeout 120s
kubectl rollout status deploy/echo-udp -n "$DEMO_NS" --timeout 120s
sleep 8

echo "==> routes report Accepted + ResolvedRefs"
for kind in tcproute/echo-tcp udproute/echo-udp; do
  accepted="$(kubectl -n "$DEMO_NS" get "$kind" \
    -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].status}')"
  assert_eq "$accepted" "True" "$kind Accepted"
  refs="$(kubectl -n "$DEMO_NS" get "$kind" \
    -o jsonpath='{.status.parents[0].conditions[?(@.type=="ResolvedRefs")].status}')"
  assert_eq "$refs" "True" "$kind ResolvedRefs"
done

echo "==> the status parentRef keeps its sectionName"
section="$(kubectl -n "$DEMO_NS" get tcproute/echo-tcp \
  -o jsonpath='{.status.parents[0].parentRef.sectionName}')"
assert_eq "$section" "echo-tcp" "TCPRoute status parentRef.sectionName"

echo "==> Gateway listeners are Programmed with their routes attached"
for name in echo-tcp echo-udp; do
  programmed="$(kubectl -n "$DEMO_NS" get gateway/l4 \
    -o jsonpath="{.status.listeners[?(@.name=='$name')].conditions[?(@.type=='Programmed')].status}")"
  assert_eq "$programmed" "True" "listener $name Programmed"
  attached="$(kubectl -n "$DEMO_NS" get gateway/l4 \
    -o jsonpath="{.status.listeners[?(@.name=='$name')].attachedRoutes}")"
  assert_eq "$attached" "1" "listener $name attachedRoutes"
done

echo "==> raw TCP echo through the gateway (TCPRoute)"
assert_eq "$(l4_probe tcp "$TCP_PORT" hello-tcproute)" "hello-tcproute" \
  "TCPRoute echo round-trip"

echo "==> raw UDP echo through the gateway (UDPRoute)"
assert_eq "$(l4_probe udp "$UDP_PORT" hello-udproute)" "hello-udproute" \
  "UDPRoute echo round-trip"

echo "==> a second route claiming the same port loses, and the first keeps serving"
kubectl apply -f - >/dev/null <<EOF
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata:
  name: echo-tcp-intruder
  namespace: $DEMO_NS
spec:
  parentRefs:
    - name: l4
      sectionName: echo-tcp
  rules:
    - backendRefs:
        - name: echo-udp
          port: $UDP_PORT
EOF
sleep 8

reason="$(kubectl -n "$DEMO_NS" get tcproute/echo-tcp-intruder \
  -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].reason}')"
assert_eq "$reason" "RouteConflict" "second claimant is refused"
still="$(kubectl -n "$DEMO_NS" get tcproute/echo-tcp \
  -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].status}')"
assert_eq "$still" "True" "the incumbent route is untouched"

# The point of settling this in the builder: the losing route must not take the
# reconcile down with it, so unrelated traffic keeps flowing.
assert_eq "$(l4_probe tcp "$TCP_PORT" still-serving)" "still-serving" \
  "traffic survives a conflicting route"

echo "==> gateway l4 e2e DONE"
