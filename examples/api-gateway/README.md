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
| [external-dns.yaml](external-dns.yaml) | **external-dns** integration | DNS from the HTTPRoute host + the controller-published `Gateway.status.addresses` (`rbac.allowStatusWrites=true`) |
| [cert-manager.yaml](cert-manager.yaml) | **cert-manager** automatic TLS | `cert-manager.io/cluster-issuer` on the Gateway + listener `certificateRefs` |

Honesty notes baked into the examples: a `RequestRedirect` rule carries no
`backendRef`; a redirect `path` target drops the request's query string;
`URLRewrite` is reported rather than half-applied — its path is measured
expressible but not wired, its `hostname` blocked by Sōzu sending two `Host`
lines (Sōzu 2.2.0, re-measured on 2.2.1 and 2.3.2), see
[docs/E2E-RESULTS.md §5c](../../docs/E2E-RESULTS.md).

Each file's header comment carries the exact `curl` to verify it. Apply one with:

```sh
kubectl apply -f examples/api-gateway/header-filter.yaml
```
