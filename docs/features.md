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
| Ingress | Rule without a host (catch-all) | ✅ | one plain-HTTP `*` frontend (Sōzu `DomainRule::Any`), emitted in `POST` position so it never shadows a specific-host route. No HTTPS frontend: a `*` is not covered by any certificate, so the host stays plain HTTP |
| Ingress | `spec.defaultBackend` | ❌ | not routed; reported as a `DefaultBackendUnsupported` problem |
| Ingress | `backend.resource` (non-Service backend) | ❌ | only Service backends |
| TLS | Termination from a `Secret` (`tls.crt`/`tls.key`) | ✅ | `type: kubernetes.io/tls` Secrets only (the controller watches nothing else); works with cert-manager-issued Secrets. Each TLS entry must list `hosts` — a hostless entry is reported (`TlsEntryWithoutHosts`) and skipped |
| TLS | SNI host selection | ✅ | handled by Sōzu |
| TLS | Wildcard certificate | ✅ | |
| TLS | Zero-gap certificate rotation | ✅ | `ReplaceCertificate` when the leaf fingerprint changes; changing only SNI names reloads the certificate with a brief TLS availability gap |
| TLS | HTTP → HTTPS redirect | ✅ | automatic for TLS-enabled Ingress hosts (301); opt out with `sozu.io/ssl-redirect: "false"` |
| Routing | Backends = pod IPs from EndpointSlice | ✅ | never the Service ClusterIP; `addressType: IPv4`/`IPv6` only — an FQDN slice is reported (`FqdnEndpointsUnsupported`) and ignored |
| Routing | Multi-port Service (match by port name) | ✅ | |
| Routing | Ready-endpoint filtering | ✅ | excludes not-ready endpoints |
| Routing | Hot reload — no proxy restart | ✅ | see [E2E-RESULTS.md](E2E-RESULTS.md) |
| Routing | Idempotent reconcile + periodic resync | ✅ | |
| Routing | Load-balancing algorithm selection | ✅ | Service annotation `sozu.io/load-balancing` (round-robin/random/least-loaded/power-of-two) |
| Routing | Sticky sessions | ✅ | Service annotation `sozu.io/sticky-sessions: "true"` |
| Routing | Per-endpoint weights | 🟡 | IR + translator support it; no standard K8s per-endpoint weight to map from |
| API gateway | Request/response header edits | ✅ | via HTTPRoute `RequestHeaderModifier`/`ResponseHeaderModifier`: `set` replaces, `add` appends, `remove` deletes; empty `set`/`add` values are rejected |
| API gateway | URL rewrite — `ReplaceFullPath` / `hostname` | 🟡 | **measured expressible** on Sōzu 2.2.0 ([E2E-RESULTS §5c](E2E-RESULTS.md)), not wired: reported as `FilterUnsupported`. Wiring it must first refuse a literal `$` (Sōzu rejects the frontend outright) and answer for the query string, which a path rewrite drops |
| API gateway | URL rewrite — `ReplacePrefixMatch` | ❌ | the compiled prefix regex's only capture group is the element boundary, so `$PATH[1]` yields `/`, not the remainder — measured |
| API gateway | Redirects — scheme + status | ✅ | `RequestRedirect`, 301/302/308 (303 and 307 have no Sōzu policy and are refused) |
| API gateway | Redirects — hostname / path / port target | ✅ | `hostname`, `path.replaceFullPath` and `port`, under 301/302/308. An unset target keeps the request's own value, and the query string is preserved. Refused, with the reason: `path.replacePrefixMatch`, a literal `$` (Sōzu reads it as a rewrite template and rejects the frontend), a redirect that changes nothing, and combining with `URLRewrite` |
| API gateway | HTTP Basic auth | 🟡 | Sōzu Cluster field; not wired (no core Gateway filter) |
| API gateway | Connection limit per source IP | ✅ | Service annotation `sozu.io/max-connections-per-ip` (a connection cap, not an RPS quota) |
| API gateway | Match on header value / query param | ❌ | not supported by Sōzu |
| API gateway | Weighted split across multiple Services | ✅ | Gateway `backendRefs` compile to a Random cluster with Service shares normalized across ready endpoints; see limitations below |
| API gateway | Request mirroring / shadowing | ❌ | not supported by Sōzu |
| Gateway API | `GatewayClass` (by `controllerName`) | ✅ | status `Accepted` reported |
| Gateway API | `Gateway.spec.infrastructure.parametersRef` | ❌ | no parameter kinds are supported; an explicit reference rejects the Gateway with `Accepted: False` / `InvalidParameters`, without programming its routes or certificates |
| Gateway API | `Gateway` HTTP/HTTPS listeners | ✅ | must declare a port the chart's `exposure` table advertises for that protocol (default `80`/`443`); a mismatch is rejected with `PortUnavailable`. A Gateway with no accepted listeners reports `Accepted: False` / `ListenersNotValid`; a mix of accepted and rejected listeners reports `Accepted: True` / `ListenersNotValid`. An unresolved certificate affects `Programmed` and `ResolvedRefs`, without rejecting the Gateway |
| Gateway API | `HTTPRoute` (host, path, method) | ✅ | status `Accepted`/`ResolvedRefs` per parent. A route whose hostnames intersect none of the listener's is `Accepted: False` / `NoMatchingListenerHostname` and does not count toward `attachedRoutes` — it is attached to nothing |
| Gateway API | Wildcard hostnames | ✅ | routing suffixes such as `*.example.com` cover one or more labels. Exact hosts take precedence, then the longest wildcard suffix. Ingress and TLS certificate wildcards still cover one label |
| Gateway API | `ReferenceGrant` (cross-namespace refs) | ✅ | gates cross-ns backend/cert refs |
| Gateway API | `allowedRoutes.namespaces` — `from: All`/`Same` | ✅ | |
| Gateway API | `allowedRoutes.namespaces` — `from: Selector` | ✅ | evaluated against Namespace labels (`matchLabels` + `matchExpressions`, ANDed; an empty selector matches every namespace). `Selector` **replaces** `Same`: the Gateway's own namespace is admitted only if its labels match. A selector this build cannot evaluate — an unknown `operator`, a malformed expression, `from: Selector` with no selector — still fails closed and is reported (`NamespaceSelectorInvalid`) |
| Gateway API | One Service `backendRef` per rule | ✅ | positive weights retain the existing Service cluster; weight zero creates a non-forwarding rule (HTTP 503 rather than the required 500) |
| Gateway API | Weighted multi-`backendRef` split | ✅ | HTTPRoute, TCPRoute and UDPRoute; zero-weight targets never receive traffic |
| Gateway API | Invalid or omitted HTTP `backendRefs` | ✅ | a missing Service/port, disallowed cross-namespace ref or unsupported kind returns HTTP 500 while preserving the match. Invalid refs report `ResolvedRefs: False`; omitted/empty refs report `True`. A resolved Service without ready endpoints retains HTTP 503. Redirects remain independent of backends; proportional rejection for weighted rules is pending integration |
| Gateway API | Header/query matches | ❌ | not supported by Sōzu |
| Gateway API | Rule-level filters (header edit, redirect) | ✅ | see the API-gateway rows above (URLRewrite reported unsupported) |
| Gateway API | Per-`backendRef` filters | ❌ | filters wire onto the frontend, not one backend; reported (`FilterUnsupported`), the rule still routes without them |
| Gateway API | `rule.timeouts` | ❌ | no Sōzu equivalent; reported (`TimeoutsUnsupported`), the rule still routes without the timeout |
| Gateway API | TLS `Passthrough` | ❌ | terminate only |
| Gateway API | `Gateway` TCP/UDP listeners | ✅ | the declared port must be a `TCP`/`UDP` entry of the chart's `exposure` table (only Helm can open a Service port); `owner` may reserve it for one namespace |
| Gateway API | `TCPRoute` / `UDPRoute` | ✅ | weighted Service `backendRefs`; a socket forwards to the oldest route with a resolved backend reference, by `creationTimestamp` then `namespace/name` (`L4RouteConflict` on other claimants); all admitted routes remain `Accepted: True` and count toward the listener's `attachedRoutes` |
| Gateway API | `GRPCRoute` / `TLSRoute` | ❌ | |
| Protocols | HTTP / HTTPS (L7) | ✅ | |
| Protocols | TCP / UDP ingress (L4) | ✅ | `TCPRoute`/`UDPRoute` only (the `tcp/udp-services` ConfigMaps are gone); one port → one route, no host routing; ports > 1024 (unprivileged), and never 443 — see below |
| Operations | Exposure via `Service type=LoadBalancer` | ✅ | |
| Operations | Structured logs (`tracing`) | ✅ | |
| Operations | Gateway API status write-back (loop-safe) | ✅ | Accepted/Programmed/ResolvedRefs |
| Operations | Ingress `status` write-back (loadBalancer) | ✅ | publishes the gateway LB address; enable with `rbac.allowStatusWrites` |
| Operations | Dedicated `/healthz` readiness gate | ✅ | `/readyz` goes green only after the first reconcile, so a Pod takes traffic only once Sōzu is programmed |

## Notes

- **HTTP error responses.** The controller serves a fixed HTTP 500 on loopback,
  and Sōzu forwards matching rejected rules to it. The port is reserved from TCP
  exposure (`controller.httpErrorPort`, default `8082`). During a controller
  restart those requests can return 503 until the responder and Sōzu's backend
  retries recover; they retain their match instead of falling through to another
  route. Complete request bodies are discarded within a five-second connection
  deadline, so malformed, stalled or oversized headers may be closed earlier.
- **Regex paths (`ImplementationSpecific`).** Sōzu 2.x anchors regexes, so a pattern that matched a
  substring on another controller may need adjusting.
- **API-gateway filters.** Header edits and redirects (scheme + status) are exposed through the IR
  and Gateway API HTTPRoute filters (Phase 3). A Gateway `set` deletes existing occurrences before
  appending its value; `add` appends and `remove` deletes, on requests and responses. Empty `set`
  or `add` values cannot be distinguished from deletion in Sōzu's protocol: the affected rule is
  skipped with `Accepted=False`, reason `UnsupportedValue`, and a `FilterUnsupported` problem.
  Other rules keep routing. Redirect host/path/port targets are **wired** — measured working on
  Sōzu 2.2.0 under every policy (see [E2E-RESULTS §5c](E2E-RESULTS.md) and [PROTOCOL.md §13](../PROTOCOL.md)),
  with a literal `$` refused in the builder because Sōzu reads it as a rewrite template and
  rejects the frontend outright, which an all-or-nothing translation turns into a failed
  reconcile for everyone. `URLRewrite` stays unwired for one measured reason: on the *forwarding*
  path a rewrite **drops the query string** that `ReplaceFullPath` keeps. (A redirect does not —
  the two share Sōzu's fields but not that behaviour.)
  The per-source-IP connection limit is wired through Service annotations (see below). HTTP Basic
  auth exists in Sōzu's data plane but has no core Gateway API filter, so it remains unwired.
- **Weighted backendRefs.** Each Service receives its declared share, divided equally across its
  ready endpoints. Integer rounding is bounded to one budget unit per Service and endpoint;
  the total stays within `i32::MAX`. Equivalent addresses are combined, and zero-weight targets
  are omitted even when every positive backend becomes unavailable. A positive share too small
  to represent for every ready endpoint is refused with `WeightedBackendsInvalid`.
  Composite clusters force Random and disable sticky sessions; they do not inherit Service
  annotations for load balancing, connection limits or retry settings. A rule with one backendRef of positive
  weight keeps its existing Service cluster and annotations. UDP chooses a backend for each new flow;
  datagrams of an established flow keep that choice.
  Invalid references set `ResolvedRefs=False`; Services without ready endpoints report
  `NoReadyEndpoints`. Their share is redistributed to ready valid references, so proportional
  HTTP 500 responses for invalid backends are **not implemented**. If no usable positive target
  remains in a composite or all-zero rule, it still matches an isolated empty cluster: HTTP returns 503 rather than the
  required 500, and TCP/UDP forward nothing. All-zero references report `NoPositiveBackendWeight`
  without incorrectly declaring valid references unresolved. Namespace grants are still checked
  for zero-weight references.
- **Hard limits.** Matching on header values or query parameters and request mirroring are not
  expressible in Sōzu today, so they are out of scope rather than merely deferred.

## Annotations

Single-Service routing is tuned with annotations on the backing **Service**, so both an Ingress
and a Gateway rule with one backendRef of positive weight share one configuration. Composite weighted
clusters use the fixed policy described above:

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

## HTTP and HTTPS exposure

The chart accepts multiple HTTP and HTTPS entries in `exposure`. Each Gateway
listener selects its advertised `(protocol, port)` and gets the corresponding
bind; HTTPS certificate stores remain separate per bind. The first entry of each
protocol serves Ingress routes. Keep these entries first when adding ports.

Sōzu creates these listeners at boot. Updating the table rolls the gateway Pods
and interrupts connections that do not finish draining before termination.

## L4 (TCP/UDP)

UDP flow identity includes the client IP and source port; clients sharing an IP
keep separate reply destinations.

Raw TCP/UDP forwarding is a `TCPRoute` or a `UDPRoute` on a layer-4 `Gateway`
listener. There is no host multiplexing at layer 4: one port forwards to exactly
one Service.

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
  `L4RouteConflict` as a diagnostic. All admitted routes remain `Accepted: True`
  and count toward the listener's `attachedRoutes`, including those whose
  backends are unresolved. A route counts once per listener even when several
  parentRefs select it. The conflict does not fail the reconcile.
- Only routes whose backend reference resolves to a Service and port contend
  for the socket. If the oldest route's Service disappears, a younger route
  with a resolved backend can receive traffic. A Service with no ready endpoints
  still holds its route's place. Reserve shared L4 ports with `owner` when
  separate namespaces must not compete for them.
- The exposure entry's optional `owner` names the only namespace whose Gateways
  may declare that port. Down here there is no hostname to arbitrate with, so
  the alternative would be a race.
- Weighted splits use the same Service-share normalization as HTTPRoute. Zero-weight targets
  are omitted; an entirely drained listener has no forwarding backend. The one-winner rule
  for a contested listener remains unchanged.
- **Ports must be > 1024, and 443 is impossible.** Both containers run as uid
  1000 with every capability dropped, and the Service already publishes 443/TCP
  for `https` — a Service cannot expose one `(port, protocol)` twice. The chart
  fails the render with that explanation rather than letting the apiserver
  reject it obscurely. Layer-4 traffic therefore lives on a port TLS clients do
  not dial by default.

The cluster + backends resolve to pod IPs exactly like HTTP, so hot reload and
pruning work the same way.

> **Removed in this release:** the `tcp/udp-services` ConfigMaps. Layer-4
> routing now requires the Gateway API CRDs — see [UPGRADING.md](UPGRADING.md)
> for why, and how to migrate.
