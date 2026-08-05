#!/usr/bin/env bash
# End-to-end test for raw TCP (L4) forwarding: a port mapped through the chart's
# l4.tcpServices is forwarded by Sōzu to a TCP echo backend. Uses bash's /dev/tcp
# so no `nc` is required.
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

PORT=9000

echo "==> context: $(kubectl config current-context)"
ensure_image
# Point the controller at the tcp-services map, and declare the port in the
# exposure table. Both are needed: the map says where the port routes, the table
# says the port exists at all — a mapping the table does not carry is reported
# (`L4PortNotExposed`) and not programmed, because the Service would have no
# port routing to it.
ensure_addon \
  --set "l4.tcpServices.${PORT}=${DEMO_NS}/echo-tcp:${PORT}" \
  --set-json "exposure=[
    {\"name\":\"http\",\"port\":80,\"bind\":8080,\"protocol\":\"HTTP\",\"transport\":\"TCP\"},
    {\"name\":\"https\",\"port\":443,\"bind\":8443,\"protocol\":\"HTTPS\",\"transport\":\"TCP\"},
    {\"name\":\"echo-tcp\",\"port\":${PORT},\"bind\":${PORT},\"protocol\":\"TCP\",\"transport\":\"TCP\"}]"
ensure_demo_ns

echo "==> deploy the TCP echo backend (examples/ingress/l4-tcp.yaml)"
kubectl apply -f "$ROOT/examples/ingress/l4-tcp.yaml" >/dev/null
kubectl rollout status deploy/echo-tcp -n "$DEMO_NS" --timeout 120s
sleep 6

echo "==> controller programmed the L4 listener?"
kubectl logs -n "$NS" deploy/"$RELEASE" -c controller --tail=40 \
  | grep -E "applying changes" | tail -2 || true

pf_start "1${PORT}:${PORT}"   # 19000 -> 9000

echo "==> raw TCP echo through the gateway"
reply=""
exec 3<>"/dev/tcp/127.0.0.1/1${PORT}"
printf 'hello-l4\n' >&3
read -r -t 5 reply <&3 || true
exec 3>&- 3<&- || true
assert_eq "$reply" "hello-l4" "TCP echo round-trip"

echo "==> l4 e2e DONE"
