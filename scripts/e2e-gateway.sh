#!/usr/bin/env bash
# End-to-end test for the Gateway API: routing + the Phase 3 HTTPRoute filters
# (header modifier, request redirect, URLRewrite hostname and full path), and a
# RegularExpression path read without the query string. Applies the shipped
# examples under examples/api-gateway/, so it also validates those manifests.
#
# The query string is checked across a path target, redirect and rewrite: Sōzu
# keeps it only on the path-only rules every match but the root prefix becomes
# (REGEX_PATH, Sōzu >= 2.3.4; see PROTOCOL.md §13).
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

echo "==> context: $(kubectl config current-context)"
ensure_image
ensure_gateway_api_crds
ensure_addon
ensure_demo_ns

echo "==> apply Gateway API examples (header, redirect and rewrite filters)"
kubectl apply -f "$ROOT/examples/api-gateway/header-filter.yaml" >/dev/null
kubectl apply -f "$ROOT/examples/api-gateway/redirect.yaml" >/dev/null
kubectl apply -f "$ROOT/examples/api-gateway/url-rewrite.yaml" >/dev/null
kubectl rollout status deploy/echo-headers -n "$DEMO_NS" --timeout 120s
kubectl rollout status deploy/echo-rewrite -n "$DEMO_NS" --timeout 120s
sleep 6

pf_start 18080:80
base="http://127.0.0.1:18080"

echo "==> RequestHeaderModifier: backend sees the injected request header"
body="$(curl -sS -D /tmp/gw-h.out -H 'Host: headers.example.com' "$base/")"
echo "$body" | grep -qi '^X-Env: prod' \
  && echo "  OK   request header X-Env: prod echoed by whoami" \
  || { echo "  FAIL X-Env not seen by backend"; exit 1; }

echo "==> ResponseHeaderModifier: client sees the injected response header"
grep -qi '^X-Served-By: sozu' /tmp/gw-h.out \
  && echo "  OK   response header X-Served-By: sozu" \
  || { echo "  FAIL X-Served-By missing from response"; exit 1; }

echo "==> RequestRedirect: HTTP -> HTTPS 301 (backend-less route)"
curl -sS -o /dev/null -D /tmp/gw-r.out -H 'Host: old.example.com' "$base/"
code="$(awk 'NR==1{print $2}' /tmp/gw-r.out)"
assert_eq "$code" "301" "redirect status"
grep -qi '^location: https://old.example.com/' /tmp/gw-r.out \
  && echo "  OK   Location: https://old.example.com/" \
  || { echo "  FAIL redirect Location wrong"; cat /tmp/gw-r.out; exit 1; }

echo "==> URLRewrite hostname: backend sees the rewritten Host"
# whoami is a Go net/http server: it answers 400 to a request carrying two Host
# lines, which is what Sōzu sent through 2.3.3, so the 200 is part of the check.
# Status, backend Host and backend X-Forwarded-Host as one observation, polled
# with a bounded request: the Deployment reading Ready says nothing about the
# controller having programmed the route yet.
rewrite_seen() {
  local code
  code="$(curl -sS --max-time 5 -o /tmp/gw-u.out -w '%{http_code}' \
    -H 'Host: rewrite.example.com' "$base/")" || return 1
  printf '%s host=%s xfh=%s' "$code" \
    "$(grep -i '^Host:' /tmp/gw-u.out | head -1 | tr -d '\r' | awk '{print $2}')" \
    "$(grep -i '^X-Forwarded-Host:' /tmp/gw-u.out | head -1 | tr -d '\r' | awk '{print $2}')"
}
wait_for "200 host=internal.example.com xfh=rewrite.example.com" \
  "host-rewrite status, backend Host and X-Forwarded-Host" rewrite_seen

echo "==> RequestRedirect ReplaceFullPath: Location keeps the query string"
# Each observation is polled with bounded requests, like the host rewrite above:
# a Deployment reading Ready says nothing about the route being programmed.
redirect_seen() {
  curl -sS --max-time 5 -o /dev/null -D /tmp/gw-rp.out \
    -H 'Host: moved.example.com' "$base/old/page?q=1" || return 1
  printf '%s %s' "$(awk 'NR==1{print $2}' /tmp/gw-rp.out)" \
    "$(grep -i '^location:' /tmp/gw-rp.out | head -1 | tr -d '\r' | awk '{print $2}')"
}
wait_for "302 http://moved.example.com/new?q=1" "path-redirect status and Location" redirect_seen

echo "==> URLRewrite ReplaceFullPath: backend sees the new path with the query"
# whoami echoes the request line it received.
request_line_seen() {
  local code
  code="$(curl -sS --max-time 5 -o /tmp/gw-line.out -w '%{http_code}' \
    -H 'Host: paths.example.com' "$base$1")" || return 1
  printf '%s %s' "$code" "$(grep -m1 -E '^(GET|POST) ' /tmp/gw-line.out | tr -d '\r')"
}
wait_for "200 GET /new?q=1&r=2 HTTP/1.1" "path-rewrite status and backend request line" \
  request_line_seen "/old/page?q=1&r=2"

echo "==> HTTPRoute RegularExpression: matched against the path, not its query"
wait_for "200 GET /api/v2?x=1 HTTP/1.1" "RegularExpression ^/api/v[0-9]+\$ on /api/v2?x=1" \
  request_line_seen "/api/v2?x=1"

echo "==> gateway e2e DONE"
