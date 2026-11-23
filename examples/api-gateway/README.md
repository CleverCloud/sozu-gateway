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
| [redirect.yaml](redirect.yaml) | **Redirect** (scheme + status; hostname, path and port targets are supported too, a path target keeping the query), backend-less routes | `RequestRedirect` |
| [url-rewrite.yaml](url-rewrite.yaml) | **Host and path rewrites**: the backend receives another `Host` (and `X-Forwarded-Host` with the original), or another path with the request's query, still from the route's own Service; and a `RegularExpression` match, read without the query | `URLRewrite` `hostname` / `path` `ReplaceFullPath` (Sōzu ≥ 2.3.4) |
| [external-dns.yaml](external-dns.yaml) | **external-dns** integration | DNS from the HTTPRoute host + the controller-published `Gateway.status.addresses` (`rbac.allowStatusWrites=true`) |
| [cert-manager.yaml](cert-manager.yaml) | **cert-manager** automatic TLS | `cert-manager.io/cluster-issuer` on the Gateway + listener `certificateRefs` |

Honesty notes baked into the examples: a `RequestRedirect` rule carries no
`backendRef`; a path target, redirect or `URLRewrite` `ReplaceFullPath`, keeps
the request's query string on every match but the root prefix `/`, Sōzu's own
prefix rule, which drops it — a redirect there is programmed and loses the
query, a `URLRewrite` path there is reported rather than half-applied, and its
rule served without any rewrite, hostname included; `ReplacePrefixMatch` is
not wired; and both rewrites need Sōzu 2.3.4 or later, as 2.3.3 and earlier
sent the backend two `Host` lines and refuse the path-only rules every match
but the root becomes. See [docs/E2E-RESULTS.md §5c](../../docs/E2E-RESULTS.md).

Each file's header comment carries the exact `curl` to verify it. Apply one with:

```sh
kubectl apply -f examples/api-gateway/header-filter.yaml
```
