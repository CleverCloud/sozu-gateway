# Gateway API examples

Examples driven by the **Gateway API** (`GatewayClass` / `Gateway` / `HTTPRoute`).
They use the `sozu-demo` namespace (create it first: `kubectl create namespace
sozu-demo`) and the GatewayClass `sozu`, and require the Gateway API CRDs to be
installed in the cluster. Hosts are fictional — reach them with a `Host` header or
`--resolve` against the gateway's LoadBalancer IP.

| File | Shows | How |
| ---- | ----- | --- |
| [gateway-api.yaml](gateway-api.yaml) | A `Gateway` (HTTP + HTTPS) + `HTTPRoute`, plus header filters and a redirect-only route | the baseline setup |
| [header-filter.yaml](header-filter.yaml) | Request/response **header edits** | `RequestHeaderModifier` / `ResponseHeaderModifier` |
| [redirect.yaml](redirect.yaml) | **Redirect** (scheme + status; hostname, path and port targets are supported too), backend-less route | `RequestRedirect` |
| [url-rewrite.yaml](url-rewrite.yaml) | **Host rewrite**: the backend receives another `Host` (and `X-Forwarded-Host` with the original), still from the route's own Service | `URLRewrite` `hostname` (Sōzu ≥ 2.3.4) |
| [external-dns.yaml](external-dns.yaml) | **external-dns** integration | DNS from the HTTPRoute host + the controller-published `Gateway.status.addresses` (`rbac.allowStatusWrites=true`) |
| [cert-manager.yaml](cert-manager.yaml) | **cert-manager** automatic TLS | `cert-manager.io/cluster-issuer` on the Gateway + listener `certificateRefs` |

Honesty notes baked into the examples: a `RequestRedirect` rule carries no
`backendRef`; a redirect `path` target drops the request's query string; a
`URLRewrite` `hostname` is wired, but a `URLRewrite` carrying a `path` is
reported rather than half-applied — the path is measured expressible but not
wired (Sōzu 2.2.0, re-measured on 2.2.1, 2.3.2 and 2.3.6), so the rule is served
without any rewrite, hostname included; and the hostname needs Sōzu 2.3.4 or
later, as 2.3.3 and earlier sent the backend two `Host` lines. See
[docs/E2E-RESULTS.md §5c](../../docs/E2E-RESULTS.md).

Each file's header comment carries the exact `curl` to verify it. Apply one with:

```sh
kubectl apply -f examples/api-gateway/header-filter.yaml
```
