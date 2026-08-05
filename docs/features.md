# Feature support

What the controller does and does not do today (Phase 1 Ingress + TLS, Phase 2 Gateway
API, Phase 3 HTTPRoute filters). It distinguishes what Sōzu
**fundamentally cannot do** from what is simply **not wired up yet**, so a hard constraint is never
mistaken for a roadmap item.

Legend: ✅ supported · 🟡 planned · ❌ not supported.

| Area | Feature | Status | Notes |
| ---- | ------- | :----: | ----- |
| Ingress | IngressClass selection (`spec.ingressClassName`) | ✅ | |
| Ingress | Legacy `kubernetes.io/ingress.class` annotation | ✅ | |
| Ingress | Default IngressClass (`is-default-class`) | ✅ | reconciles class-less Ingresses |
| Ingress | Host match — exact | ✅ | |
| Ingress | Host match — wildcard (`*.example.com`) | ✅ | one extra label |
| Ingress | `pathType: Prefix` | ✅ | |
| Ingress | `pathType: Exact` | ✅ | |
| Ingress | `pathType: ImplementationSpecific` | ✅ | mapped to a Sōzu regex (2.x anchors regexes) |
| Ingress | Multiple Ingresses / hosts / paths | ✅ | de-duplicated by route key; a conflicting owner of the same host+path is reported (`RouteCollision` on the loser; the winner is deterministic) |
| Ingress | Rule without a host (catch-all) | ❌ | skipped with a reported problem |
| Ingress | `spec.defaultBackend` | ❌ | not routed; reported as a `DefaultBackendUnsupported` problem |
| Ingress | `backend.resource` (non-Service backend) | ❌ | only Service backends |
| TLS | Termination from a `Secret` (`tls.crt`/`tls.key`) | ✅ | `type: kubernetes.io/tls` Secrets only (the controller watches nothing else); works with cert-manager-issued Secrets. Each TLS entry must list `hosts` — a hostless entry is reported (`TlsEntryWithoutHosts`) and skipped |
| TLS | SNI host selection | ✅ | handled by Sōzu |
| TLS | Wildcard certificate | ✅ | |
| TLS | Zero-gap certificate rotation | ✅ | `ReplaceCertificate` |
| TLS | HTTP → HTTPS redirect | ✅ | automatic for TLS-enabled Ingress hosts (301); opt out with `sozu.io/ssl-redirect: "false"` |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP; `addressType: IPv4`/`IPv6` only — an FQDN slice is reported (`FqdnEndpointsUnsupported`) and ignored |
| Routing | Multi-port Service (match by port name) | ✅ | |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | ✅ | Service annotation `sozu.io/load-balancing` (round-robin/random/least-loaded/power-of-two) |
| Routing | Sticky sessions | ✅ | Service annotation `sozu.io/sticky-sessions: "true"` |
| Routing | Per-endpoint weights | 🟡 | IR + translator support it; no standard K8s per-endpoint weight to map from |
| API gateway | Request/response header edits | ✅ | via HTTPRoute `RequestHeaderModifier`/`ResponseHeaderModifier` (Sōzu has no append → `add` applied as set) |
| API gateway | URL rewrite (host + full path) | ❌ | `URLRewrite` reported unsupported: Sōzu's `rewrite_host` rewrites the *backend authority* (dials the rewritten host) → route 408s |
| API gateway | Redirects (scheme + status) | ✅ | `RequestRedirect` (HTTP→HTTPS, 301/302); host/path/port target not yet |
| API gateway | HTTP Basic auth | 🟡 | Sōzu Cluster field; not wired (no core Gateway filter) |
| API gateway | Connection limit per source IP | ✅ | Service annotation `sozu.io/max-connections-per-ip` (a connection cap, not an RPS quota) |
| API gateway | Match on header value / query param | ❌ | not supported by Sōzu |
| API gateway | Weighted split across multiple Services | ❌ | not supported by Sōzu |
| API gateway | Request mirroring / shadowing | ❌ | not supported by Sōzu |
| Gateway API | `GatewayClass` (by `controllerName`) | ✅ | status `Accepted` reported |
| Gateway API | `Gateway` HTTP/HTTPS listeners | ✅ | must declare a port the chart's `exposure` table advertises for that protocol (default `80`/`443`); a mismatch is rejected with `PortUnavailable`. Status `Accepted`/`Programmed` |
| Gateway API | `HTTPRoute` (host, path, method) | ✅ | status `Accepted`/`ResolvedRefs` per parent |
| Gateway API | `ReferenceGrant` (cross-namespace refs) | ✅ | gates cross-ns backend/cert refs |
| Gateway API | `allowedRoutes.namespaces` — `from: All`/`Same` | ✅ | |
| Gateway API | `allowedRoutes.namespaces` — `from: Selector` | ❌ | fails closed — the listener admits no routes; reported as `NamespaceSelectorUnsupported`. A controller gap, not a Sōzu limit: evaluating selectors needs a Namespace watch |
| Gateway API | One Service `backendRef` per rule | ✅ | a single ref with `weight: 0` (drain) is rejected (`ZeroWeightBackendUnsupported`): Sōzu cannot express the spec's all-zero-weight 500 |
| Gateway API | Weighted multi-`backendRef` split | ❌ | not supported by Sōzu |
| Gateway API | Header/query matches | ❌ | not supported by Sōzu |
| Gateway API | Rule-level filters (header edit, redirect) | ✅ | see the API-gateway rows above (URLRewrite reported unsupported) |
| Gateway API | Per-`backendRef` filters | ❌ | filters wire onto the frontend, not one backend; reported (`FilterUnsupported`), the rule still routes without them |
| Gateway API | `rule.timeouts` | ❌ | no Sōzu equivalent; reported (`TimeoutsUnsupported`), the rule still routes without the timeout |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `Gateway` TCP/UDP listeners | ✅ | the declared port must be a `TCP`/`UDP` entry of the chart's `exposure` table (only Helm can open a Service port); `owner` may reserve it for one namespace |
| Gateway API | `TCPRoute` / `UDPRoute` | ✅ | one Service `backendRef`; a socket carries exactly one route, and a second claimant loses on `creationTimestamp` then `namespace/name` (`L4RouteConflict`) — never by failing the reconcile |
| Gateway API | `GRPCRoute` / `TLSRoute` | ❌ | |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | TCP / UDP ingress (L4) | ✅ | `TCPRoute`/`UDPRoute` (or the deprecated `tcp/udp-services` ConfigMaps); one port → one Service, no host routing; ports > 1024 (unprivileged), and never 443 — see below |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Gateway API status write-back (loop-safe) | ✅ | Accepted/Programmed/ResolvedRefs |
| Operations | Ingress `status` write-back (loadBalancer) | ✅ | publishes the gateway LB address; enable with `rbac.allowStatusWrites` |
| Operations | Dedicated `/healthz` readiness gate | ✅ | `/readyz` goes green only after the first reconcile, so a Pod takes traffic only once Sōzu is programmed |

## Notes

- **Regex paths (`ImplementationSpecific`).** Sōzu 2.x anchors regexes, so a pattern that matched a
  substring on another controller may need adjusting.
- **API-gateway filters.** Header edits and redirects (scheme + status) are exposed through the IR
  and Gateway API HTTPRoute filters (Phase 3). Sōzu has no header *append*, so a Gateway `add` is
  applied as a set; redirect host/path/port targets are not expressible yet and are reported rather
  than half-applied. `URLRewrite` is reported unsupported: Sōzu's `rewrite_host`/`rewrite_path`
  rewrite the *backend authority* (the proxy dials the rewritten host) and expect regex-capture
  templates, which is incompatible with the Gateway semantics (rewrite the forwarded Host/path
  toward the same backend) — a literal mapping makes the route time out (408).
  The per-source-IP connection limit is wired through Service annotations (see below). HTTP Basic
  auth exists in Sōzu's data plane but has no core Gateway API filter, so it remains unwired.
- **Hard limits.** Matching on header values or query parameters, weighted traffic split across
  several Services, and request mirroring are not expressible in Sōzu today, so they are out of
  scope rather than merely deferred.

## Annotations

Cluster-level routing is tuned with annotations on the backing **Service** (a cluster is 1:1 with a
Service, so both an Ingress and a Gateway route to that Service share one configuration):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/load-balancing` | `round-robin`, `random`, `least-loaded`, `power-of-two` | `round-robin` | Sōzu load-balancing algorithm for the cluster. Unknown values fall back to the default. |
| `sozu.io/sticky-sessions` | `"true"` / `"false"` | `"false"` | Pin a client to one backend via a Sōzu sticky cookie. |
| `sozu.io/max-connections-per-ip` | integer | global default | Cap simultaneous connections from one source IP to this cluster. Over the cap → `429`. A non-numeric value is ignored. |
| `sozu.io/retry-after` | integer (seconds) | unset | `Retry-After` header sent on that `429`. |

One annotation is read from the **Ingress** instead (it depends on that Ingress's TLS, not the Service):

| Annotation | Values | Default | Effect |
| ---------- | ------ | ------- | ------ |
| `sozu.io/ssl-redirect` | `"true"` / `"false"` | `"true"` | Redirect HTTP→HTTPS (`301`) for hosts that have a loaded cert. Auto-on; set `"false"` to keep serving plain HTTP. (Gateway API uses an explicit `RequestRedirect` filter instead.) |

## L4 (TCP/UDP)

Raw TCP/UDP forwarding has **two** front ends. There is no host multiplexing at
layer 4 either way: one port forwards to exactly one Service.

### TCPRoute / UDPRoute (supported)

Declare the port in the chart's `exposure` table — only Helm can open a port on
the Service — then point a `Gateway` listener and a route at it:

```yaml
exposure:                        # values.yaml
  - { name: http,     port: 80,   bind: 8080, protocol: HTTP,  transport: TCP }
  - { name: https,    port: 443,  bind: 8443, protocol: HTTPS, transport: TCP }
  - { name: postgres, port: 5432, bind: 5432, protocol: TCP,   transport: TCP, owner: demo }
```

```yaml
listeners:                       # Gateway
  - { name: postgres, protocol: TCP, port: 5432 }
---
rules:                           # TCPRoute
  - backendRefs: [{ name: postgres, port: 5432 }]
```

A full example is in [`examples/api-gateway/l4-routes.yaml`](../examples/api-gateway/l4-routes.yaml).

- A socket carries exactly one route. Two routes claiming it are settled by
  oldest `creationTimestamp`, then `namespace/name`; the loser reports
  `L4RouteConflict` on its own status and **the rest of the routing is
  untouched** — a port dispute between two tenants must never fail the reconcile
  for everyone else.
- The exposure entry's optional `owner` names the only namespace whose Gateways
  may declare that port. Down here there is no hostname to arbitrate with, so
  the alternative would be a race.
- Weighted splits and `weight: 0` drains are refused exactly as they are for
  HTTPRoute — Sōzu cannot express either.
- **Ports must be > 1024, and 443 is impossible.** Both containers run as uid
  1000 with every capability dropped, and the Service already publishes 443/TCP
  for `https` — a Service cannot expose one `(port, protocol)` twice. The chart
  fails the render with that explanation rather than letting the apiserver
  reject it obscurely. Layer-4 traffic therefore lives on a port TLS clients do
  not dial by default.

### `tcp/udp-services` ConfigMaps (deprecated)

The ingress-nginx convention, pointed to by `--tcp-services-configmap` /
`--udp-services-configmap` (Helm `l4.tcpServices` / `l4.udpServices`):

```yaml
# ConfigMap data — "<gateway-port>": "<namespace>/<service>:<service-port>"
data:
  "5432": "demo/postgres:5432"   # TCP :5432 -> the postgres Service
```

It still works, and will be removed. The reason is not tidiness: the map is
cluster-global and has no admission control whatsoever — anyone who can edit it
routes any port to any Service in any namespace, with no ReferenceGrant and no
Gateway to consent. Where a route and an entry name the same socket, the route
wins and the entry is reported (`L4PortClaimedByRoute`).

Both paths resolve the cluster + backends to pod IPs exactly like HTTP, so hot
reload and pruning work the same way. An entry mapping a port the `exposure`
table does not carry is reported (`L4PortNotExposed`) and not programmed: the
Service would have no port routing to it, so a listener nobody can dial reads as
working while serving nobody.
