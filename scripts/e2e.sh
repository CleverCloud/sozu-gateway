#!/usr/bin/env bash
# End-to-end test: deploy the sozu-gateway add-on + a demo Ingress app on the
# current kube-context and assert what Sōzu answers, through a port-forward to
# the gateway Pod:
#   - a host without a cert proxies plain HTTP (200) and reaches whoami;
#   - a cert-covered host redirects HTTP to HTTPS (301) and serves *our* cert
#     over HTTPS (200); `sozu.io/ssl-redirect: "false"` opts out, hot;
#   - `pathType: Prefix` matches on element boundaries (/foo/bar, not /foobar);
#   - rotating the TLS Secret swaps the served cert;
#   - deleting the Ingress withdraws its routes (404) and its cert, and leaves
#     the other Ingress serving.
# Every transition is polled to a bound (`wait_for`), never slept through.
# Companion suites: e2e-gateway.sh (Gateway API + filters),
# e2e-l4-routes.sh (TCPRoute/UDPRoute).
#
# The controller image is pushed to an ephemeral, anonymous registry (ttl.sh) by
# default so this works without registry credentials. Export IMAGE to use your
# own registry (e.g. IMAGE=ghcr.io/you/sozu-gw-controller:test).
set -euo pipefail
source "$(dirname "$0")/e2e-lib.sh"

HOST="app.example.com"           # the shipped demo Ingress: TLS + auto redirect
PLAIN_HOST="plain.example.com"   # no cert, so plain HTTP proxies
PREFIX_HOST="prefix.example.com" # routes only /foo, to observe the boundary
HTTP="http://127.0.0.1:18080"
HTTPS_PORT=18443
# The TLS host: SNI and Host pinned to $HOST while the socket goes to the
# port-forward; -k because the cert is self-signed.
HTTPS="https://$HOST:$HTTPS_PORT"
TLS_ARGS=(-k --resolve "$HOST:$HTTPS_PORT:127.0.0.1" -H "Host: $HOST")

# HTTP status of one request, "000" when nothing came back (refused, reset,
# no cert for the SNI), so a poll sees an observation rather than an error.
http_code() {
  local code rc
  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 "$@" 2>/dev/null)"
  rc=$?
  # curl can print a status line and still exit non-zero if it then fails
  # mid-body (a timeout, a reset). That is not a completed response, so it is
  # `000`, not the code it managed to print — otherwise a flaky backend reads
  # as a success.
  if [ "$rc" -ne 0 ]; then echo 000; else echo "${code:-000}"; fi
}

# The SHA-256 fingerprint Sōzu serves for $HOST's SNI, empty when the handshake
# yields no certificate. `timeout` bounds it: `openssl s_client` has no deadline
# of its own, and `wait_for` only checks its own bound *between* probe calls, so
# an unbounded handshake could hang the whole suite.
served_fingerprint() {
  timeout 5 openssl s_client -connect "127.0.0.1:$HTTPS_PORT" -servername "$HOST" </dev/null 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256 2>/dev/null || true
}

# "yes" when the cert Sōzu serves for $HOST's SNI is exactly the PEM in $1.
serves_cert() {
  if [ "$(served_fingerprint)" = "$(openssl x509 -noout -fingerprint -sha256 -in "$1" 2>/dev/null)" ]; then
    echo yes
  else
    echo no
  fi
}

# "yes" once none of the certificates in $@ is the one served for $HOST's
# SNI *and something else is*, on every one of several consecutive
# handshakes. Sōzu never answers an unmapped SNI with no certificate: it falls
# back to its built-in default, so "no certificate at all" never happens and
# cannot be the withdrawal signal. "Not ours" alone is not enough either: a
# transport error reads that way, and so would a worker that fell back to the
# *previous* test certificate — hence every certificate this run issued is
# excluded, not only the current one. And one handshake is not enough: each
# Sōzu worker (`sozu.workerCount`) holds its own certificate table and applies
# the removal on its own, a fresh connection lands on any of them, so a single
# default-certificate answer says only that *one* worker has withdrawn it.
# Withdrawal is therefore: a certificate is served, it is none of ours, and it
# stays that way across enough connections to have reached every worker.
CERT_WITHDRAWN_SAMPLES=8
cert_withdrawn() {
  local served ours sample
  local -a excluded=()
  for ours in "$@"; do
    excluded+=("$(openssl x509 -noout -fingerprint -sha256 -in "$ours" 2>/dev/null)")
  done
  for sample in $(seq "$CERT_WITHDRAWN_SAMPLES"); do
    served="$(served_fingerprint)"
    [ -n "$served" ] || { echo no; return; }
    for ours in "${excluded[@]}"; do
      if [ "$served" = "$ours" ]; then
        echo no
        return
      fi
    done
  done
  echo yes
}

# Self-signed cert for $HOST into $1.crt / $1.key, applied as the Secret the
# demo Ingress names. A fresh key every time, so a rotation is a new fingerprint.
issue_cert() {
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$1.key" -out "$1.crt" -days 365 \
    -subj "/CN=$HOST" -addext "subjectAltName=DNS:$HOST" 2>/dev/null
  kubectl create secret tls app-tls -n "$DEMO_NS" --cert="$1.crt" --key="$1.key" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
}

# Release the routing objects this suite creates, pass or fail, so a rerun
# cannot pass on what an earlier run left behind. The whoami Deployment and
# Service are shared demo fixtures (examples/api-gateway/gateway-api.yaml
# applies the same ones), and the `app-tls` Secret is what those examples
# expect to find after a run, so both stay.
cleanup() {
  if [ "$1" -ne 0 ]; then
    echo "--- controller log (last 30 lines) ---"
    kubectl logs -n "$NS" deploy/"$RELEASE" -c controller --tail=30 2>/dev/null || true
  fi
  kubectl -n "$DEMO_NS" delete --ignore-not-found ingress/whoami ingress/whoami-plain \
    >/dev/null 2>&1 || true
  [ -n "${WORK:-}" ] && rm -rf "$WORK"
}
on_exit cleanup

echo "==> context: $(kubectl config current-context)"
ensure_image
ensure_addon
ensure_demo_ns

echo "==> deploy demo app + TLS secret"
kubectl apply -f "$ROOT/examples/ingress/demo-app.yaml" >/dev/null
WORK="$(mktemp -d)"
issue_cert "$WORK/first"
# A second Ingress on the same Service: a host with no cert, where plain HTTP
# must proxy rather than redirect, and a host routing only /foo, where the
# prefix boundary can be observed (with / routed too, /foobar would match it).
kubectl apply -f - >/dev/null <<EOF
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: whoami-plain
  namespace: $DEMO_NS
spec:
  ingressClassName: sozu
  rules:
    - host: $PLAIN_HOST
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: whoami
                port:
                  number: 80
    - host: $PREFIX_HOST
      http:
        paths:
          - path: /foo
            pathType: Prefix
            backend:
              service:
                name: whoami
                port:
                  number: 80
EOF
kubectl rollout status deploy/whoami -n "$DEMO_NS" --timeout 120s

pf_start 18080:80 "$HTTPS_PORT:443"

echo "==> a host without a cert proxies plain HTTP"
wait_for 200 "GET $PLAIN_HOST/ over HTTP" http_code -H "Host: $PLAIN_HOST" "$HTTP/"
curl -sS --max-time 5 -o "$WORK/body.out" -H "Host: $PLAIN_HOST" "$HTTP/"
assert_grep '^Hostname: whoami-' "$WORK/body.out" "body comes from a whoami pod"
assert_eq "$(http_code -H "Host: nobody.example.com" "$HTTP/")" 404 "unknown host is a 404"

echo "==> a cert-covered host redirects HTTP to HTTPS and serves our cert"
wait_for 301 "GET $HOST/ over HTTP" http_code -H "Host: $HOST" "$HTTP/"
curl -sS --max-time 5 -o /dev/null -D "$WORK/redirect.out" -H "Host: $HOST" "$HTTP/"
# Compare the Location value literally: a regex would read $HOST's dots as
# any-char and, unanchored, accept a trailing path Sōzu never sent.
location="$(tr -d '\r' <"$WORK/redirect.out" | awk 'tolower($1) == "location:" { print $2 }')"
assert_eq "$location" "https://$HOST/" "Location is exactly https://$HOST/"
wait_for 200 "GET $HOST/ over HTTPS" http_code "${TLS_ARGS[@]}" "$HTTPS/"
wait_for yes "served certificate is the one in the Secret" serves_cert "$WORK/first.crt"

echo "==> pathType: Prefix matches on element boundaries"
wait_for 200 "GET $PREFIX_HOST/foo" http_code -H "Host: $PREFIX_HOST" "$HTTP/foo"
assert_eq "$(http_code -H "Host: $PREFIX_HOST" "$HTTP/foo/bar")" 200 "/foo covers /foo/bar"
assert_eq "$(http_code -H "Host: $PREFIX_HOST" "$HTTP/foo?q=1")" 200 "/foo covers /foo?q=1"
assert_eq "$(http_code -H "Host: $PREFIX_HOST" "$HTTP/foobar")" 404 "/foo does not cover /foobar"
assert_eq "$(http_code -H "Host: $PREFIX_HOST" "$HTTP/")" 404 "/foo does not cover /"

echo "==> sozu.io/ssl-redirect: \"false\" keeps serving plain HTTP, hot"
kubectl -n "$DEMO_NS" annotate ingress/whoami sozu.io/ssl-redirect=false --overwrite >/dev/null
wait_for 200 "GET $HOST/ over HTTP with the opt-out" http_code -H "Host: $HOST" "$HTTP/"
assert_eq "$(http_code "${TLS_ARGS[@]}" "$HTTPS/")" 200 "HTTPS unaffected by the opt-out"
kubectl -n "$DEMO_NS" annotate ingress/whoami sozu.io/ssl-redirect- >/dev/null
wait_for 301 "GET $HOST/ over HTTP once the opt-out is removed" http_code -H "Host: $HOST" "$HTTP/"

echo "==> rotating the TLS Secret swaps the served certificate"
issue_cert "$WORK/second"
wait_for yes "served certificate is the rotated one" serves_cert "$WORK/second.crt"
assert_eq "$(http_code "${TLS_ARGS[@]}" "$HTTPS/")" 200 "HTTPS still answers after the rotation"

echo "==> deleting the Ingress withdraws its routes and cert; the other keeps serving"
kubectl -n "$DEMO_NS" delete ingress/whoami >/dev/null
wait_for 404 "GET $HOST/ over HTTP after the delete" http_code -H "Host: $HOST" "$HTTP/"
wait_for yes "certificate withdrawn from the HTTPS listener (default served)" cert_withdrawn "$WORK/first.crt" "$WORK/second.crt"
assert_eq "$(http_code -H "Host: $PLAIN_HOST" "$HTTP/")" 200 "$PLAIN_HOST unaffected by the delete"

echo "==> e2e DONE"
