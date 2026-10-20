# Upgrading

Breaking changes, what they cost, and what to do about them. Newest first.

---

## Sōzu 2.3.2

The controller now links `sozu-command-lib` 2.3.2 and the chart defaults to
`clevercloud/sozu:2.3.2` (was 2.2.1). **The controller and the Sōzu image move
together.** The builder now refuses tenant input the way the 2.3.2 main process
does (below), and the translator spells out what a 2.3 cluster removal does
implicitly. An older controller in front of a 2.3 data plane admits
certificates and path regexes that 2.3.2 refuses (measured), and translation is
all-or-nothing, so one such object fails every reconcile of the shared instance.
Pinning `image.sozu.tag` below 2.3.0 under this controller is unsupported too:
route moves and teardowns were measured to converge on that pairing, nothing
else was tested, and everything below describes 2.3.

Changing the image rolls the gateway Pods; keep at least two replicas. If your
values pin `image.sozu.tag`, or you upgrade with `--reuse-values`, set
`--set image.sozu.tag=2.3.2`. The IR and the persisted shadow format are
unchanged.

### What you must check

**Certificates 2.3.2 refuses.** Sōzu now pairs a certificate with its key
through rustls before loading it, in the main process and in every worker, and
the controller runs the same check: a leaf that rustls-webpki cannot parse as an
end-entity certificate, or whose SubjectPublicKeyInfo is not byte-for-byte the
one derived from `tls.key`, is refused. That is the rule, not a list; measured
refused by 2.3.2 and served by 2.2.1, for instance: an X509v1 leaf, an EC leaf
carrying a compressed point, an RSA key whose algorithm parameters are absent
instead of `NULL`, an outer signatureAlgorithm that differs from the signed
one, an unknown critical extension, and a leaf over 64 KiB of DER. Such
a Secret is reported (`InvalidCertificate`; a Gateway listener goes
`ResolvedRefs: False`) and programs nothing: the hosts it covered **lose their
HTTPS frontend**, and an Ingress host its automatic HTTP→HTTPS redirect, while
plain HTTP keeps routing. An HTTPRoute `RequestRedirect` on the HTTP listener is
built without looking at certificates, so it keeps redirecting to a host that no
longer has an HTTPS frontend (read from the code, not measured).
Re-issue those certificates, and check
`kubectl get events -A --field-selector reason=InvalidCertificate` after the
upgrade.

**User path regexes are anchored.** Sōzu 2.3.0 compiles every `Regex` path rule
as `\A(?:…)\z` and matches it against the whole request target, query string
included (upstream 47eb07c). The rules the controller generates for `Prefix` and
`Exact` were already full-span and route as before. An `ImplementationSpecific`
path or HTTPRoute `RegularExpression` written as a substring or a suffix changes
meaning — measured on 2.3.2: `/api/v[0-9]+` no longer matches `/api/v1/x` or
`/api/v1?q=1`, `\.js$` matches nothing, `/q` no longer matches `/q?x=1`. Rewrite
them full-span as the [path-rule entry](#path-rules-are-written-for-sōzu-23s-anchored-router-as-well-as-221)
below shows. A pattern that compiles bare but not once wrapped — an `(?x)`
comment that swallows the closing group, nesting within two levels of the
parser's limit (248 or 249 nested groups) — was accepted by 2.2.1 and is
refused by 2.3.2; the controller now reports it
(`InvalidPathRegex`) and skips that match instead of failing every reconcile.

**Host matching moves traffic.** Two router changes from 2.3.0, measured on both
versions with frontends laid out as the controller emits them:

- The `Host` header is matched case-insensitively. On 2.2.1,
  `Host: FOO.EXAMPLE.COM` matched no host route: it went to the hostname-less
  catch-all (an Ingress rule without `host`, a hostname-less HTTPRoute) when one
  existed, and was answered `404` when none did. It now reaches
  `foo.example.com`'s routes. A `Host` with a trailing dot still matches no
  host, on either version.
- An exact host whose rules miss the request's path or method now falls through
  to a covering one-label `*.` wildcard host before the catch-all. With
  `foo.example.com` serving `/api` and `*.example.com` serving `/bar`,
  `GET foo.example.com/bar` went on 2.2.1 to the catch-all when there was one
  and got a `404` when there was none; on 2.3.2 it reaches the wildcard's
  Service. A method miss falls through the same way.

A catch-all backend therefore loses its uppercase-`Host` and exact-host-miss
traffic. This is fall-through, not isolation: routes of different Gateway
listeners sharing one Sōzu listener still answer each other's misses (see
[features.md](features.md)). It also widens a known Gateway API deviation
([#78](https://github.com/CleverCloud/sozu-gateway/issues/78)): an HTTPRoute
rule dropped for an invalid `backendRef` on an exact host, which the spec
answers `500`, now falls through to a covering `*.` route and gets a `200` from
another Service, where 2.2.1 went to the catch-all or answered `404` (inferred
from the measured fall-through).

**HTTP connect timeouts fail over, and exhaustion is `503`.** On the HTTP and
HTTPS listeners, a backend that does not accept within `sozu.timeouts.connect`
(2 s) is now a failed connect like a refused one (upstream e959838e): the
backend enters back-off and the request moves to a backend it has not tried,
within `max_connection_attempts` (default 5; 2.2.1 retried only refusals, 3
times). Measured locally (the chart's Sōzu config, one worker) with one live
and one black-holed backend: 2.2.1
answered half the requests `504` after ~2 s each; 2.3.2 answered all of them
`200`, only the first to touch the dead backend paying ~2 s. When every attempt
fails the answer is `503`, no longer `504` — a `504` remains possible when an
HTTP/1 request's front timeout fires mid-failover, while on an HTTP/2
connection the bound is attempts × connect timeout (10 s here).

- An alert that reads `504` as "backend unreachable" has to watch `503` instead
  (`sozu_http_503_errors`, `sozu_backend_connect_retries_exhausted`; both
  per-cluster, so `metrics.perCluster` or the access logs).
- A back-off window reopens: each time it expires the dead backend is tried
  again, and every request load-balanced to it meanwhile pays the connect
  timeout before failing over — repeated latency bursts, not a single hit.
- A Service with one ready backend gets runs of immediate `503`s while that
  backend is in back-off, even if it recovered in between; 2.2.1 simply dialled
  again. Measured on 2.3.2 with every backend black-holed: the first request
  took 10 s (5 × 2 s), the second 6.2 s, later ones failed in ~0.3 ms.
- A sticky client is re-pinned after a single connect timeout of its backend
  (two lost SYNs, a briefly full accept queue), not only when the backend is
  gone.
- **TCPRoute is unchanged.** A connect timeout still closes the session, with
  no failover and no back-off, and the backend is never marked down (measured
  identical on both versions).

**Idempotent requests can run twice.** A request written onto a pooled
keep-alive backend connection that the backend closed before sending a byte got
a `502` on 2.2.1. Sōzu 2.3 re-sends it on another backend connection when its
method is idempotent (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE` —
never `POST` or `PATCH`), up to four more times (`max_connection_attempts`, 5),
to the same backend or another: selection runs again and nothing excludes the
stale one. Measured with a single backend: `GET` and `PUT` got `200` on 2.3.2
where 2.2.1 answered `502`; `POST` still got `502`. A `PUT` or `DELETE` the
first backend processed before dropping the connection runs twice. It happens
whenever a backend closes a pooled keep-alive before Sōzu reuses it — a backend
idle timeout shorter than Sōzu's reuse, or a rolling update (inferred; no
rollout was measured).

**SIGTERM now stops Sōzu.** 2.2.x ignored SIGTERM, which is why the chart's
`preStop` hook runs `sozu shutdown`. 2.3.0 soft-stops on SIGTERM (upstream
57c92699), and a second SIGTERM while the workers are still stopping hard-stops
them. Measured with `docker stop`: ~15.8 s ending in SIGKILL on 2.2.1, ~0.16 s
and a clean exit on 2.3.2, an in-flight HTTP/1 request still answered. With the
default chart little changes: the hook still waits `sozu.drain.delaySeconds` so
the Pod leaves the endpoints before the listeners close, then soft-stops; a 2.3
`sozu shutdown` waits for the drain itself and ignores `-t` (still passed,
inert), so the grace period is the only bound, and if it runs out the kubelet's
SIGTERM hard-stops the workers (measured locally; expect a `FailedPreStopHook`
Event, as the 2.2.1 timeout gave). **`sozu.drain.enabled: false` changes
meaning:** deleting a
Pod is now fast and in-flight HTTP/1 requests drain, but the listeners close at
once, so new connections are refused while the endpoint removal propagates —
that delay is what the hook still buys.

**The per-IP cap never reaches a UDPRoute.** Sōzu 2.3.0 applies a cluster's
`max_connections_per_ip` to UDP as a cap on concurrent flows per source address
(upstream 99e6aee2), silently dropping new flows over it, and counts each client
source port as a flow, so clients behind a NAT — or behind the node SNAT of
`externalTrafficPolicy: Cluster` — would lose new flows. The controller
therefore never sends the cap on a UDP cluster: `sozu.io/max-connections-per-ip`
keeps capping HTTP requests and TCPRoute sessions and does nothing on a
UDPRoute, as on 2.2.1. The edge case: the cap is per cluster, one per Service
port, so a Service port that a UDPRoute also uses loses the cap for its TCPRoute,
HTTPRoute and Ingress traffic too (read from the code, not measured).

### What changes on its own

- **`power-of-two` spreads load.** Under equal load, 2.2.1 sent every request to
  the last two backends it scanned and none to the rest; measured locally
  (docker, one worker, four backends, 400 sequential requests): 0/0/208/192 on
  2.2.1, 106/100/107/87 on 2.3.2 (upstream a4f09532). Such Services rebalance
  across all their Pods on upgrade.
- **Weights.** Every policy now honours a backend's weight, and a weight of `0`
  counts as `1` (`random` reads the raw value). The controller sets none, so
  selection is unchanged.
- **Certificate name changes apply in place** ([#81](https://github.com/CleverCloud/sozu-gateway/issues/81),
  fixed upstream in 0bc160b7): a same-fingerprint `ReplaceCertificate` whose
  names changed now updates every worker's SNI table (measured). The translator
  already sends it, so name changes on an unchanged certificate no longer need
  a Pod roll.
- **Repeated UDP listener activation keeps live flows.** On 2.2.1, re-sending
  `ActivateListener` for an active UDP listener answered `OK` and silently cut
  every established flow (measured); the controller re-sends it when it retries
  a failed batch that had added a UDP listener, or reconciles from an unreadable
  shadow against a running Sōzu. 2.3.2 keeps the flows (measured).
- **HTTP/1 smuggling hardening.** A request with neither `Content-Length` nor
  `Transfer-Encoding` no longer swallows the request pipelined behind it (on
  2.2.1 that request reached the first one's backend unrouted and unedited,
  measured); a signed `Content-Length` (`+5`) is answered `400` from a client
  and `502` from a backend; forbidden and spoofed trailer fields (`Host`,
  `Content-Length`, `Authorization`, `X-Forwarded-For`, …) are elided, and more
  than 128 trailer fields is a `400`. A response sent while the request body is
  still arriving now closes the client connection.
- **HTTP/2.** PINGs no longer extend the front timeout: a client sending only
  PINGs is closed at `front` (2.2.1 kept it open, measured), and a request
  still waiting for its first response byte when `front` expires gets `504`
  even if the client PINGs — keep `sozu.timeouts.front` ≥ `back` (the defaults,
  60 and 30, are). Flood thresholds are 20× higher, an answer Sōzu generates
  (`404`, `429`, `503`) ends only its stream instead of the connection, and a
  client `GOAWAY` no longer cuts in-flight responses.
- **Forwarding headers.** 2.3 adds a forwarded-header mode whose default,
  `both`, keeps 2.2.1's headers, but a client
  `Forwarded` line that is not well-formed RFC 7239 is now removed instead of
  extended, and HTTP/2 request trailers drop `X-Forwarded-Proto`, `-Port` and
  `-Host`. A backend that parses `Forwarded` sees the difference.
- **Smaller ones.** The `Sozu-Id` header and a generated `X-Request-Id` now
  change on every request of a keep-alive connection (measured; they were per
  connection). An HTTPRoute `method` match is case-sensitive on the wire: `get`
  no longer matches `GET` (measured, `404`). A non-persistent HTTP/1.0 backend
  now closes the client connection after each response. During a soft stop, an
  HTTP/1.1 response still in flight carries `Connection: close`.
- **Memory outside the buffer pool grows** (read from the source, not
  measured): idempotent-request replay captures take up to 512 × 16 400 bytes
  per worker, about 8.4 MB carried and up to about 16.8 MB allocated — at the
  chart's `max_buffers` of 20000 that ceiling, not the pool, is what binds; and
  the default HTTP/2 connection window is now 16 MiB (was 1 MiB), so a stalled
  HTTP/2 upload can hold about 6.25 MiB of kernel receive buffer at 100 streams,
  against about 1 MiB before (bounded by `net.ipv4.tcp_rmem`). Leave headroom
  above the pool budget.
- **Metrics.** The default scrape exports the same 49 families as on 2.2.1
  (measured on both, [2.3.2 run](probes/metrics-no-clusters_sozu-2.3.2_2026-10-19.txt)).
  Separately, `sozu_socket_read_infinite_loop_error`, which appeared only once it
  had fired, no longer exists — an alert on it now reads "no data". New proxy counters appear
  once they first fire (TCP and TLS read/write resets,
  `sozu_h2_flood_stream_refused`, the trailer counters,
  `sozu_http_close_request_incomplete`, `sozu_backend_retry_captures_*`).
  `metrics.perCluster` adds `sozu_backend_header_time` (and `_histogram`) and
  `sozu_backend_retry_stale_upstream`. The per-cluster worker wedge described
  [below](#metrics-exports-proxy-wide-sōzu-series-only) is **still present on
  2.3.2** ([measured](probes/metrics-no-clusters_sozu-2.3.2_2026-10-19.txt), the
  message about 12% larger), so `perCluster` stays off by default.
- **The image** runs on Alpine 3.24 instead of Alpine edge; the entrypoint, the
  paths and the shell the `preStop` hook uses are unchanged (checked), and
  `/tmp` still needs its volume.
- **Stale watches.** `--watch-timeout-secs` stays required. The 14.7–19.7% loss
  quoted [below](#watch-blindness-is-bounded-by-default) was measured on Sōzu
  2.2.1, where every failure was a `504` against a withdrawn backend. On 2.3.2
  the same blindness is expected to show as failover latency bursts and `503`s
  instead; that is not measured.

### Downgrading

Move both images back together. The image change rolls the Pods, which
restarts Sōzu, as [the downgrade procedure](#downgrading-the-controller)
requires; the shadow format is unchanged.

---

## `sozu.io/sticky-sessions` pins clients, with an opaque cookie

The annotation never pinned anyone. Sōzu writes the backend's `sticky_id` into
the `SOZUBALANCEID` cookie, or its backend id when there is none, but looks a
returning cookie up by `sticky_id` alone; no backend carried one, so every
request was load-balanced as usual and the cookie was re-issued whenever the
backend changed. The cookie also exposed `<namespace>.<service>.<port>#<pod IP>:<port>`
to every client. Each backend of a sticky Service now carries a 16-hex-character
id, an HMAC of its backend id keyed by the Service UID: identical on every
replica and across restarts, unique among the backends of one Service port, and
opaque to a client, which never learns the UID. Pods joining or leaving leave
the other backends' ids alone, barring a truncated-HMAC collision (odds around
n²/2⁶⁵ per Service port), where the uniqueness rule can move one backend's id.

The first reconcile after the upgrade re-sends every backend of a sticky
Service once. Sōzu applies it in place (measured on 2.2.1 and 2.3.2), in the
main process and in each worker, keeping connections and retry state, so the
change needs no Sōzu restart of its own; a chart upgrade that ships it rolls the
gateway Pods anyway. The cookie follows more slowly: Sōzu issues it when it
selects a backend, and a request on a connection whose backend connection is
still open reuses that backend without selecting again, so such a client can
keep its old cookie until that connection closes (on Sōzu 2.2.x an HTTP/1
keep-alive client could also be re-sent it, a later response on the connection
repeating the earlier `Set-Cookie`; fixed in 2.3). The next request that
does select a backend load-balances an old cookie, which no longer matches
anything, and sets the new one. The id changes when the Service is deleted and
recreated, which re-pins its clients the same way. Two sticky Services on
different paths of one hostname share the single `SOZUBALANCEID` cookie
(`Path=/`): switching between them can overwrite a client's pin and trigger
fresh load balancing. Non-sticky Services are untouched.

---

## Sōzu's buffer pool is sized to its connection limit

Sōzu's config set `max_connections = 10_000` per worker but left `max_buffers`
at Sōzu's default of 1000, and every HTTP/1 connection or TCPRoute session
holds two buffers for as long as it is open, idle keep-alives included. A
worker therefore stopped near 500 connections: past that, a new connection was
accepted and closed at once while the Pod stayed Ready. Measured on 2.2.1 with
two workers: 1,000 idle keep-alive clients made every fresh request fail; at
the new default, 6,000 were held and fresh requests were still served.

The chart now renders `max_buffers` from **`sozu.maxBuffers`, default 20000**
(twice the connection limit), so for HTTP/1 and TCP traffic `max_connections`
is the limit that binds. HTTP/2 needs more: a connection holds one buffer plus
two per stream slot it has allocated, and a finished stream's slot is kept for
reuse, so a connection that once ran N concurrent streams can keep holding
1 + 2N. Raise `sozu.maxBuffers` if many clients speak HTTP/2.

**What it costs: memory under load.** A buffer is ~16 KiB, committed the first
time it is used and kept until the worker restarts, so the buffer pool can now
grow to `workerCount × maxBuffers × 16 KiB` — about 630 MiB with the defaults;
the previous buffer-pool capacity was about 32 MiB. That is the pool's budget,
not Sōzu's ceiling: TLS sessions and other connection state are allocated
outside it and need headroom on top. Idle memory is unchanged. If
`resources.sozu` carries a memory limit, size it against your real peak or
lower `sozu.maxBuffers`; otherwise a connection surge becomes an OOM kill.

Sōzu reads the value only at boot. The rendered config changes, so
`helm upgrade` rolls the gateway Pods; `--reuse-values` from an older release
gets the new default too.

`sozu.maxConnectionsPerIp` is new as well: the default for the per-(Service
port, client IP) cap that the `sozu.io/max-connections-per-ip` annotation
overrides.
It defaults to `0`, unlimited — Sōzu's own default — so nothing changes unless
you set it. An HTTP request over the cap is answered `429`, which Sōzu counts
per Service port: the default `/metrics` (proxy-wide series only) does not show
these rejections; the access logs do, and so does `metrics.perCluster: true`.
Neither the default nor the annotation reaches a UDPRoute: since the move to
Sōzu 2.3.2 the controller sends no cap on a UDP cluster, which 2.3 would apply
as a silent per-source flow cap (see [Sōzu 2.3.2](#sōzu-232)).

---

## `/metrics` exports proxy-wide Sōzu series only

A scrape used to ask every Sōzu worker for all of its per-cluster and
per-backend metrics, in one message per worker. On Sōzu 2.2.1 a worker whose
message exceeds `max_command_buffer_size` (1 638 400 bytes in the chart)
requeues it forever: it stops serving traffic and commands and spins a core,
while `/readyz`, the container probes and `reconcile_failures_total` all stay
green, until the Pod is restarted. It also logs an error on every requeue —
1.6 million lines in about 20 s on 2.2.1; on 2.3.2 tens of thousands within a
minute, counted in a log capped at 20 MB — which the chart's
`log_target = "stdout"` writes to the node's container logs. This is not new in
2.3, and Sōzu 2.3.2 still does all of this
([measured](probes/metrics-no-clusters_sozu-2.3.2_2026-10-19.txt)), and its
message is about 12% larger: 63 984 bytes at 80 clusters against 56 889 on 2.2.1,
same probe and buffers. The message grows
with every routed Service port that has received traffic. On a laptop, on
2.2.1, with the chart's
buffers, two workers and traffic on every cluster, the full query still
answered at 2 000 clusters and wedged both workers at 2 500 and at 3 000
([runs 3–5](probes/metrics-no-clusters_sozu-2.2.1_2026-09-23.txt)), which
puts each cluster's share at 650–820 bytes with the probe's short cluster
names. Take that as an order of magnitude, not a limit: longer
`namespace.service.port` names, backend detail and how traffic spreads over
the workers all move it, and no production gateway was measured. A routine
scrape could therefore take a large gateway down.
Filtering the query by cluster does not bound it either: at backend detail,
which `sozu top` switches on at runtime, one cluster grows with its backends.
Scrapes now ask for process-level metrics only
([measured](probes/metrics-no-clusters_sozu-2.2.1_2026-09-23.txt)).

**What disappears by default** — every series labelled `cluster_id`. At Sōzu's
default detail level these families go entirely:

- `sozu_requests`, and the status counters of *routed* requests:
  `sozu_http_status_2xx`, `…_5xx`, per-code `sozu_http_status_<code>`,
  `sozu_http_<code>_errors` (502, 503, 504, …);
- `sozu_backend_response_time`, `sozu_backend_connection_time` (and their
  `_histogram`), `sozu_backend_connections_error`,
  `sozu_connections_per_backend`;
- `sozu_cluster_available_backends`, `sozu_cluster_total_backends`;
- `sozu_frontend_matching_time` (and `_histogram`).

`sozu_bytes_in`/`_out`, `sozu_request_time`, `sozu_service_time` (and their
`_histogram`) and `sozu_access_logs_count` keep their unlabelled proxy series
and lose the per-cluster ones. What stays: the proxy series (connections,
`sozu_http_requests`, buffers, event loop, and the 404s Sōzu answers itself as
`sozu_http_status_404`), the `process="main"` configuration gauges, and every
`sozu_gw_controller_*` signal. **An alert or dashboard built on the removed
series now reads "no data", not zero** — a 5xx-rate alert on
`sozu_http_status_5xx` goes silent. A side effect: the endpoint no longer
lists every tenant's `namespace.service.port`.

**To opt back in**, knowing the risk above, set `metrics.perCluster: true`
(`--metrics-per-cluster`, `SOZU_GW_METRICS_PER_CLUSTER=true`). There is no
safe Service count to aim for: the figures above only say that hundreds of
Services are far from the edge and a few thousand are at it. If you enable it
on a gateway that routes more than a few hundred, split the Services across
Gateway instances rather than tuning against a number.

---

## Routes are arbitrated on the key Sōzu stores them under

Sōzu 2.2.1 stores an HTTP(S) route under the unescaped string
`{address};{hostname};{rule}[;{method}]`, so a regex path containing `;` can
spell another route's method: an HTTPRoute `RegularExpression` `/x;GET` with no
method and a `/x` match restricted to `GET` on the same host are two routes to
the gateway but one to Sōzu. Such a pair used to reach the translator
unarbitrated and fail **every** reconcile of the shared instance — no route,
endpoint or certificate change applied for any tenant — until one of the two
was deleted. The collision identity is now that exact string: the oldest
claimant wins, as for any `RouteCollision`, and the loser is reported
(`Accepted: False`, reason `RouteCollision`, plus a Warning Event). Two
matches of **one** route that only share the key are reported the same way,
since the second is not served.

Nothing to do unless such a pair exists; on upgrade the stuck instance
converges and the loser shows up in its status. A regex that needs a literal
`;` without clashing can write it `[;]` or `\x3B`. The IR and shadow format
are unchanged.

---

## Hostnames Sōzu cannot parse are refused per object

Kubernetes checks a hostname against the RFC 1123 label grammar only; Sōzu also
runs it through IDNA (UTS #46) and rejects the frontend when that fails. An
apiserver-valid name such as `xn--a.example.com`, or a digit-led label in a
domain with a right-to-left label (`0.xn--4gbrim.example.com`), therefore used
to fail **every** reconcile, which prevented the shared instance from
converging, and a new or restarted Pod never became Ready, until the object
was deleted.

Such a name is now refused in the controller, with a Warning Event
`InvalidHostname` on the object that carries it, and only the frontends that
would carry it are skipped. On an Ingress, the rule with that `host` is
skipped. On an HTTPRoute, the hostname is dropped and the route stays
`Accepted`, as with an uncompilable regex. A Gateway listener whose own
`hostname` cannot be parsed is `Accepted: False` / `UnsupportedValue`, and
routes attach to it no more than to any other refused listener. One subtlety
follows Sōzu exactly: in a right-to-left domain the `*` label itself fails, so a
`*.xn--4gbrim.example.com` listener keeps serving the names beneath it, and
only a frontend carrying the wildcard is refused.

Nothing to do on upgrade. These names never served traffic; fix them to a
valid A-label (for example, `xn--bcher-kva.example.com` for `bücher`).
Certificate names are not affected: Sōzu loads them without IDNA processing.

---

## Path rules are written for Sōzu 2.3's anchored router as well as 2.2.1

Sōzu 2.2.1 matches a `Regex` path rule unanchored; 2.3.0 and later wrap every
such rule in `\A(?:…)\z` (upstream 47eb07c, marked BREAKING), so a rule must
span the whole request target, query string included. (Written when that was
still unreleased; the anchoring is in effect since the
[move to 2.3.2](#sōzu-232).) The rules a
`Prefix` and an `Exact` path compile to were written for the first behaviour
only — `^/foo(/|\?|$)` stops at the boundary — and would have matched neither
`/foo/bar` nor `/v1?x=1` on the new router: a data-plane bump would have turned
most routes into 404s. They are now **full-span**, `^/foo(?:[/?](?-u:.*))?$` and
`^/v1(?:\?(?-u:.*))?$`, which mean the same thing under both routers; the tail
is raw bytes (`-u`) so a target Sōzu's tolerant HTTP/1 parser admits is not
narrower than before. Routing on 2.2.1 is unchanged for every request one
rule decides. Where a user regex overlaps a `Prefix` or `Exact` on the same
host — both match `/foo` — 2.2.1 applies whichever rule was added last, the
add order follows the rule text, and the text changed, so such a pair can swap
winners. That precedence was never defined here; a configuration that leans on
it should give one of the two a path the other does not match.

**Roll the gateway Pods after upgrading**, for the reason the `Exact` entry
below gives: the persisted state holds the Kubernetes path, not the compiled
rule, so an unchanged route produces no migration request and keeps its old
spelling in Sōzu. That spelling still routes on 2.2.1 — but a later removal
is issued in the new spelling, finds nothing, and is tolerated as already
gone, leaving the old rule serving. A fresh Sōzu starts clean.

**User regexes are handed to Sōzu verbatim and change meaning with it.** An
`ImplementationSpecific` path or an HTTPRoute `RegularExpression` match written
for 2.2.1 as `/api` (substring) or `\.js$` (suffix) stops matching on 2.3.
Full-span patterns — `^/api(?:[/?](?-u:.*))?$`,
`^(?-u:.*)\.js(?:\?(?-u:.*))?$` — behave identically on both; `(?-u:.*)` rather
than `.*` so a remainder that is not valid UTF-8 reads the same both ways too.
An Ingress path must start with `/`; a regex still anchors its start behind a
slash matched zero times, `/{0}^/api…`, which the apiserver accepts. See
[features.md](features.md).

One consequence of the new spelling: a user regex that spelled the *previous*
internal form of an `Exact` or `Prefix` rule (`^/foo(?:\?|$)`, `^/foo(/|\?|$)`)
was one Sōzu route with that path and arbitrated as a `RouteCollision`; it is
now a second, overlapping route, and which one answers `/foo` is Sōzu's rule
order rather than the collision policy. Rewrite it as the plain `Exact` or
`Prefix` path it meant.

Inferred certificate names (a Gateway listener without `hostname`) are now
stored **lowercase**, as rustls presents the SNI: a SAN spelled
`MiXeD.Example.COM` was previously programmed verbatim and selected by no
handshake on 2.2.1 (Sōzu 2.3.0 lowercases names at load the same way, 704b60d;
the controller keeps doing it so the main process's record is lowercase too).
The name set of such a certificate changes, so its `ReplaceCertificate` is
emitted on the first reconcile after the upgrade — a same-fingerprint replace,
which 2.2.1's worker ignores, so on 2.2.1 the mapping takes effect on the Pod
roll above; 2.3.2 applies it in place.

---

## `pathType: Exact` is an anchored regex, no longer Sōzu's `Equals`

Measured on Sōzu 2.2.1: an `Equals` rule is compared against the request
target with its query string, so `/get?x=1` did not match an `Exact /get`
route (404); and the worker's rule equality has no `Equals` arm, so a
`RemoveHttpFrontend` for such a route was acknowledged while the rule kept
matching — a deleted `Exact` route stayed reachable. `Exact` now compiles to
a whole-path regex (`^<escaped path>(?:\?(?-u:.*))?$` since the entry above),
which has neither defect. The trailing slash is
kept literal (Exact means exact). Because the rule is now a regex, an `Exact`
path long enough to exceed the regex engine's compiled-size limit (a few
hundred kilobytes, which Kubernetes does not bound) is refused and reported
(`InvalidPathRegex`) rather than forwarded — the same guard a non-root `Prefix`
gets, since it always compiled to a regex.

**Roll the gateway Pods after upgrading.** The diff renders both sides through
the new mapping, so an unchanged `Exact` route produces no migration request,
and the `Equals` rules already loaded in 2.2.x workers cannot be removed by any
request anyway; only a fresh Sōzu starts clean. (This is 2.2.x history: Sōzu
2.3.0 can remove an `Equals` rule, 795b95d, but still compares it against the
query-bearing target, so `Exact` stays a regex, and the move to 2.3.2 restarts
Sōzu regardless.) An `ImplementationSpecific`
regex that spells the same anchored pattern as an `Exact` or `Prefix` path on
the same host is now reported as a `RouteCollision` instead of being silently
dropped by the translator. The winner is decided exactly as for any route-key
clash — for two Ingresses, the existing lexicographic cluster-id order; for
HTTPRoutes, oldest `creationTimestamp` then name — this change only makes the
two spellings recognise each other as the same Sōzu route.

---

## Invalid keys and regex paths are reported, not applied

Two tenant inputs used to reach Sōzu unchecked and were measured to fail every
reconcile of the shared instance until the offending object was removed — a
new route created during the freeze stayed 404: a `tls.key` whose PEM body is
not a private key, and a regex path (Ingress `pathType: ImplementationSpecific`,
HTTPRoute `type: RegularExpression`) that Sōzu cannot compile.

Both are now refused in the builder and reported on the object that carries
them — `InvalidCertificate` (Ingress Event, listener `ResolvedRefs: False` with
`InvalidCertificateRef`) and `InvalidPathRegex` (a Warning Event on the owning
object). A rejected certificate programs nothing for that Secret; an
uncompilable regex match is dropped like an unsupported header/query match, so
the rule's other matches, the route's other rules and every other object still
program, and the route stays `Accepted`.

Two inputs that used to load now stop: a `tls.key` that is a valid key but does
not belong to `tls.crt` (Sōzu 2.2.x never checked the pair, and every handshake
for those names failed; 2.3.2 refuses it, see [Sōzu 2.3.2](#sōzu-232)), and an
HTTPRoute rule with an uncompilable regex match,
which is skipped whole — its other matches included — rather than half-applied.
Check `kubectl get events` and route status after upgrading for either reason.
## The watch bound is the controller's default, not only the chart's

`--watch-timeout-secs` (`SOZU_GW_WATCH_TIMEOUT_SECS`) now defaults to `60` in
the binary; `controller.watchTimeoutSecs: 60` in the chart merely mirrors it.
Until now the binary defaulted to `0` and the chart rendered the env var only
for a truthy value, so any release whose values predate the key — every
`helm upgrade --reuse-values` from such a release, which was the documented
upgrade command — silently ran unbounded, with the
[14.7–19.7% loss measured on Sōzu 2.2.1](#watch-blindness-is-bounded-by-default)
that the setting exists to prevent. An install that never set the key now gets the
bound on its next image roll, with no values change.

**`0` still opts out.** It keeps kube-rs's own default of 290 s, in both the
routing controller and the provisioner (which used to read `0` as `60`). The
chart renders the env var whenever the key is *set*, zero included, so
`controller.watchTimeoutSecs: 0` reaches the container; only an absent key
leaves the binary's default in force.

**`295` and above are refused at startup.** kube-rs rejects a watch
`timeoutSeconds` of 295 or more on every watch *start* — after the initial
LIST has already filled the cache — and the controller's watch loop only
warned about it, so such a value did not crash: every cache froze on its LIST
snapshot behind a green `/readyz`, the exact blindness the flag bounds. The
controller now exits with an error naming the limit instead.
## Teardown failures are no longer all tolerated

The agent used to skip *any* `Failure` on a `Remove*`/`DeactivateListener`,
so a removal Sōzu refused for a real reason — a request it could not
interpret, a worker that could not act — still advanced the shadow over an
object Sōzu kept serving. Only the "no longer held" answers are skipped now
(`Did not find`, `did not bring any change`, `found no listener`, `the
listener is not activated`, `no TCP|UDP listener to remove`, `Could not
remove route`), checked per worker so one worker's benign answer cannot hide
another's failure — and an aggregate in which every worker says `OK` is a
worker timeout, not an absent object, so it fails too. Any other teardown failure fails the reconcile, which is
then retried from the unchanged shadow.

Two related changes in the same release: a command-socket connection is
dropped after *every* channel error, including one on the reconnect-and-retry
(the protocol has no request ids, so a late reply on a kept connection would
be read as the next request's ack), and a batch the controller has given up
on stops between two requests instead of landing its remainder on the socket.
The agent's per-read deadline is 20 s (was 30 s) so that a request's usual
worst case stays under the controller's 60 s apply deadline and the real
error is what gets logged.
## The persisted shadow carries Sōzu's restart generation

`/run/sozu/shadow.json` is now `{"generation": …, "ir": …}` — the last-applied
IR together with the command-socket identity and worker PIDs of the Sōzu it
was applied to. At startup the file is resumed only when the Sōzu found
answers with the same socket identity; a Sōzu that restarted while the
controller was down is otherwise indistinguishable from one that kept its
state, because with the static HTTP/HTTPS listeners a fresh Sōzu already dumps
four records to `save_state` — the previous emptiness probe could never say
"empty". A file in the old bare-`Ir` format carries no such proof and is
ignored. On a controller-only restart that discards a legacy file, the new
empty baseline can only emit *adds*, so anything deleted from Kubernetes while
the old controller was down stays in Sōzu until an unrelated change re-diffs
it. **Roll the gateway Pods (both containers) when upgrading onto this format**,
so Sōzu starts empty and the first apply is authoritative; a controller-only
restart is safe on every later start, once a file in the new format exists.

Two behaviours changed with it. The shadow advances and is persisted the
moment the socket apply succeeds, before the status and Event calls that
follow: a failing apiserver could otherwise leave Sōzu ahead of the baseline
and every later pass re-emitting the same delta (each frontend answered
`Exists`, then removed and re-added — a routing gap per route). And a worker
that restarts on the same socket no longer resets the shadow: the main
process re-feeds its state to the new worker, and a reset diffs `empty →
desired`, which emits only adds — an object that left the desired state in
between would never be removed. On a reset the Pod also leaves the Service
(`/readyz` drops) until the re-apply succeeds, so a Sōzu that came back with
only its static listeners does not keep taking traffic it would answer 404/503. A new liveness
tick (`--sozu-probe-secs` / `controller.sozuProbeSecs`, default 2 s, `0` disables) runs the
generation check on a timer so an idle cluster notices a restart in ~2 s rather than at the next
resync; it does one `Status` round-trip and reconciles only when it finds a change —
or when a previous reconcile failed and its work is still pending: a transient
socket error during an apply is then retried on the next tick that reaches Sōzu,
rather than waiting for an unrelated Kubernetes event (which, with the resync
disabled, might never come).

---

## Automatic Gateway instances

Enable automatic provisioning once for the Helm release:

```yaml
gatewayProvisioning:
  enabled: true
  # Optional: otherwise inherit replicaCount and Service settings.
  replicaCount: 2
  service:
    type: LoadBalancer
```

Every Gateway whose GatewayClass names this controller then gets a dedicated
controller + Sōzu Deployment, Service and ConfigMap in the release namespace.
Enabled disruption budgets and metrics resources are provisioned with it. Adding
a Gateway requires no Helm change. The original Deployment and Service continue
serving Ingress; they remain the only writers of Ingress and GatewayClass status.
There is no list of Gateway names. The earlier, unreleased `gatewayInstances`
setting is rejected so it cannot silently stop isolating configured Gateways.
The Gateway API CRDs must already be installed. Remove any manually scoped
Gateway deployments before enabling provisioning to avoid duplicate workers for
the same Gateway. Creating a Gateway on this class allocates workloads and,
with the default Service type, a LoadBalancer in the release namespace; grant
Gateway creation rights with that infrastructure cost in mind.

Provisioning is disabled by default to preserve existing addresses and routing
on upgrade. Enabling it moves all owned Gateways to new Services; disabling it
moves their routes back to the shared instance. Neither migration is atomic.
Allow for LoadBalancer provisioning, watch `Programmed` and update DNS or clients
before relying on the new addresses. Private `ClusterIP` Services publish their
internal addresses. Pending LoadBalancers never publish their ClusterIP as an
external address. NodePort address publication is not implemented.

The provisioner runs separately with one replica and a `Recreate` update strategy.
Its write permissions cover infrastructure only in the release namespace;
workers retain their existing routing permissions and cannot create workloads.
Each worker still keeps its own Kubernetes caches and programs its co-located
Sōzu through the local command socket. This change automates infrastructure; a
shared routing controller and remote configuration transport are separate work.

Instances inherit images, resources, exposure, scheduling, drain, TLS hardening,
timeouts and metrics settings from the release. `gatewayProvisioning.replicaCount`
and `gatewayProvisioning.service` override the generated instances only. An
explicit Service field replaces that field, including maps: `annotations: {}`
clears inherited annotations. Listener ports still use the chart's `exposure`
table; automatic provisioning does not add support for arbitrary HTTP binds.

Names and ownership distinguish the installation UID and Gateway UID. Recreating
a Gateway under the same namespace/name creates a new instance; an old worker
cannot claim the replacement. The provisioner reconciles on Gateway/Class changes
and checks infrastructure drift every `controller.resyncSecs` (60 by default;
0 disables periodic checks). Failed operations retry after five seconds. It
checks current API identity before updating or removing resources and refuses
to adopt a foreign object with a colliding name.
Gateway deletion or loss of class ownership removes that Gateway's generated
resources. The Helm-managed template ConfigMap owns them within the release
namespace, so uninstalling the release also triggers Kubernetes garbage
collection. An absent provisioner delays Gateway cleanup until it returns.
Recreating the template ConfigMap, including a `fullnameOverride` change,
changes the installation UID: its old instances are collected and replacements
receive new names and addresses. Preserve that ConfigMap's identity during
ordinary upgrades.
Edit the template through Helm. Direct ConfigMap edits leave the running
provisioner NotReady until a Helm upgrade or a restart of the provisioner
Deployment reloads the template; it does not reload mounted changes in place.

Each Pod has its own command socket and persisted shadow. Route status updates
preserve the full parent references and entries from other instances. Service
addresses are published only by the worker that owns the corresponding Gateway.
The shadow format is unchanged. Existing NetworkPolicies and monitors selecting
only the default Deployment's labels need selectors for generated Pods too;
`sozu.io/installation-uid` and `sozu.io/gateway-uid` identify those instances.

---

## Gateway certificate name inference

HTTPS listeners sharing a certificate now retain the DNS names inferred by a
listener without `hostname`, alongside the explicit names of other listeners.
Invalid or empty inferred names reject only the listener requesting inference.

On Sōzu 2.2.1, roll the gateway Pods when adopting this change so Sōzu loads
the corrected name set. Restarting only the controller cannot repair the names
of an already loaded certificate there: 2.2.1 acknowledges replacement of the
same fingerprint without updating its SNI names. The same held for adding or
removing a hostname-less listener, or changing explicit listener hostnames,
while retaining the same certificate.

Sōzu 2.3.2 applies such a same-fingerprint `ReplaceCertificate` in place
([#81](https://github.com/CleverCloud/sozu-gateway/issues/81), fixed upstream
in 0bc160b7), and the controller already sends it, so since the
[move to 2.3.2](#sōzu-232) none of these changes needs a Pod roll. Rotation to a
certificate with a different fingerprint was never affected.

---

## HTTPRoute collision precedence

For HTTPRoute rules that emit frontends with the same protocol, listener,
hostname, path kind/value and method, the oldest route wins, then the first
alphabetical `namespace/name` on a timestamp tie. For these identical matches
within one route, the first rule wins without rejecting that route for its
own overlap. Backend names and redirects do not influence which HTTPRoute
wins. Prefixes such as `/api` and `/api/` share one collision key, as their
trailing slash is insignificant; exact paths retain that distinction.

A previously colliding route may therefore change backend at the first
reconcile. **Ingress candidates now use the same policy** — oldest
`creationTimestamp`, then `namespace/name` — instead of the old lexicographic
cluster-id order, and it applies uniformly across Ingress and HTTPRoute. This
closes a cross-namespace host takeover: a tenant could previously win another
namespace's `host+path` just by having a backend whose cluster id
(`{namespace}.{service}.{port}`) sorted earlier. The winner is now whoever
claimed the route first. Two consequences on upgrade: an install relying on
the old cluster-id order may see a different Ingress win a contested
`host+path` (the loser is reported with `RouteCollision`), and the winner no
longer depends on the backend Service's namespace.

Rules skipped because of unsupported or unresolved configuration do not reserve
a route key. Collisions between different objects still report the final winner
on the losing object's own parent or Ingress result; prefix paths in those
reports use their canonical spelling. The IR and persisted shadow format are
unchanged.

This correction selects between frontends sharing one key. Matching precedence
between different keys and the translator's incremental update strategy remain
unchanged. Native path precedence and route update limitations are tracked in
[#80](https://github.com/CleverCloud/sozu-gateway/issues/80).

---

## UDP client identity

UDP routes now use both the client IP and source port as the Sōzu flow key.
Clients behind the same IP can use separate sockets without receiving another
socket's replies. Backend selection remains stable for the lifetime of each flow;
a new source port starts a separate flow.

Roll out the complete gateway Pods when upgrading. A controller-only restart
with an unchanged persisted IR does not replace an already installed cluster's
UDP settings. A Pod rollout recreates the UDP listeners and drops existing flows.
The persisted IR format is unchanged.

---

## Gateways with infrastructure parameters

A Gateway that sets `spec.infrastructure.parametersRef` is now rejected with
`Accepted: False` / `InvalidParameters`. No parameter kinds are supported by this
controller. Earlier versions silently ignored the reference and served its routes
without applying the requested configuration.

The rejected Gateway contributes no routes or certificates to Sōzu, and its
listeners report `Programmed: False` with no attached routes. Other Gateways
continue to be configured. Omit `parametersRef` only when the deployment's
existing controller and chart configuration is the configuration you intend.

---

## Sōzu 2.2.1

The chart now defaults to `clevercloud/sozu:2.2.1` (was `2.2.0`). The controller's
`sozu-command-lib` was already pinned to `2.2.1` and stays there. The protobuf
schema, channel framing and routing diff are unchanged between these versions;
no controller configuration or persisted-shadow migration is needed.

[Upstream 2.2.1](https://github.com/sozu-proxy/sozu/releases/tag/2.2.1) fixes
frontend validation and command failure handling, and redacts sensitive debug
and error output. Existing header and rewrite limitations are unchanged.

Changing the image rolls the gateway Pods. Keep at least two replicas during
the rollout. If your values pin `image.sozu.tag`, or you use `--reuse-values`,
set `--set image.sozu.tag=2.2.1` to adopt the new image.

See the [upgrade validation](E2E-RESULTS.md#sozu-221-upgrade-validation-2026-09-10)
for the measured results and coverage limits.

---

## The data plane defaults to two replicas

`replicaCount` was `3`. It is now `2`. Nothing else changes: the same hard
hostname spread, the same `maxUnavailable: 1` budget.

Three was chosen so that losing a Pod never empties the Service. Two does that
already, and the difference was measured rather than assumed. Forcing the loss of
one gateway Pod — which is what a rolling node replacement does, one worker at a
time — cost a single replica **4.8%** and **8.0%** of fresh connections in two
trials, and cost two and three replicas **nothing**, on the same cluster with the
same probes. The third replica earned its keep only when a *second* loss
overlapped the first: two Pods destroyed at once cost the two-replica deployment
3.2% while the three-replica one stayed clean.

So the third replica buys tolerance of a concurrent second failure, not
protection against ordinary maintenance. It is worth keeping if you want that
margin, and worth its capacity to say so:

```yaml
replicaCount: 3
```

Two replicas are also the safer default on small clusters, where the hard
hostname spread has fewer domains to work with.

**Nothing needs doing on upgrade** unless you set `replicaCount` yourself, in
which case your value still wins. A release that adopts the new default rolls one
Pod out; the budget and the drain keep that gap-free.

---

## Sōzu is drained on shutdown, and the grace period grows to 40s

The data-plane container gets a `preStop` hook and
`terminationGracePeriodSeconds: 40` (was Kubernetes' default 30).

Sōzu 2.2.x registers no signal handler, and a signal with no handler is
discarded when it reaches PID 1 of a container — so SIGTERM did nothing and
every Pod deletion sat out the full grace period before being SIGKILLed, losing
the response to whatever was in flight. Measured on a three-node cluster, on
2.2.x: **31–32s to delete a gateway Pod before, 6–7s after**, the difference
being the 5s delay plus about a second of draining. Sōzu 2.3.0 soft-stops on
SIGTERM; the hook is still what delays the stop until the Pod has left the
endpoints, see [Sōzu 2.3.2](#sōzu-232).

**The first upgrade is not drained.** The outgoing Pod is the old spec, so it
still waits out its own grace period; only Pods created from this chart onwards
drain.

**`--reuse-values` needs care.** Helm replays the previous release's values over
the new chart and does not pick up keys the chart has since added. The chart
defaults `sozu.drain` internally so this no longer fails, but
`--reset-then-reuse-values` is the flag that actually gives you the new defaults.

**Sizing under `externalTrafficPolicy: Local`** — the chart's default. The 5s
delay covers kube-proxy and the CNI, not an external load balancer, which stops
sending only once its own health check fails (often 10–30s). Before this change
Sōzu kept accepting for the whole 30s grace and covered that window by accident;
now the listener closes at 5s and connections arriving after that are refused.
If you depend on an external LB, raise `sozu.drain.delaySeconds` to at least its
depool time.

**What is drained** is HTTP/1 exchanges in progress, for the whole budget.
HTTP/2 streams are force-closed 5 s into the soft stop (Sōzu's default
`h2_graceful_shutdown_deadline_seconds`, which the chart does not set; measured
on 2.2.1 and 2.3.2). TCPRoute and UDPRoute sessions, WebSockets, TLS handshakes
in progress and idle keep-alives are cut once the delay elapses.

Set `sozu.drain.enabled: false` to opt out entirely; the grace period then
returns to Kubernetes' default too. On 2.3 the kubelet's SIGTERM then
soft-stops Sōzu at once, without the delay: the listeners close at SIGTERM, so
connections arriving while the endpoint removal propagates are refused, and
in-flight HTTP/1 exchanges still drain.

## Prometheus metrics are served by default

`metrics.enabled` now defaults to `true`. A `helm upgrade` with unchanged values
therefore adds a `ClusterIP` Service, opens a container port, and rolls the Pod.

**What it exposes.** The endpoint has no authentication and no NetworkPolicy, and
the series carry `cluster_id` as `namespace.service.port` — so any Pod that can
reach the Service can enumerate the routed Services of every tenant. Backend Pod
IPs are not exposed at Sōzu's default detail level. (Since
[the entry above](#metrics-exports-proxy-wide-sōzu-series-only), the `cluster_id`
series and the per-Service counters below exist only with
`metrics.perCluster: true`.) On a shared cluster, put a NetworkPolicy in front
of it or turn it off:

```yaml
metrics:
  enabled: false
```

**What it costs.** A scrape is a `QueryMetrics` on the same command socket that
carries routing applies, and that socket is served by a single worker in order.
A slow scrape can therefore delay — and in the worst case fail — a reconcile.
Two known limits, neither of which the chart can fix on its own:

- the scraper stops waiting after 10s and returns `503`, but the query it started
  keeps the socket busy for longer than that;
- an `AggregatedMetrics` payload larger than `max_command_buffer_size`
  (1 638 400 bytes) is rejected by Sōzu, which closes the connection instead of
  answering. Large clusters will see scrapes fail deterministically. (Worse, a
  single worker's share over that limit wedges the worker — see
  [the entry above](#metrics-exports-proxy-wide-sōzu-series-only), which made
  the per-cluster query opt-in.)

**What it is good for.** The controller's own signals — the
last-successful-reconcile timestamp above all — plus Sōzu's request counters and
status codes. Note what it was *not* good for on Sōzu 2.2.x:
`cluster.available_backends` against `cluster.total_backends` only moves when
Sōzu's passive circuit breaker trips, and 2.2.x arms it only on a *refused*
connection, so a backend that has gone silent is still counted available. From
2.3.0 an HTTP connect timeout counts as a failure too, and repeated timeouts
mark a silent backend down; a silent backend behind a TCPRoute still never is.
## The backend connect timeout drops to 2 seconds

The chart used to set no timeouts, so Sōzu's own applied — including **3 seconds
to connect to a backend**. It now renders `connect_timeout = 2`.

A `helm upgrade` picks this up like any other config change — unless you
upgrade with `--reuse-values`, which replays the previous release's values in
place of the new chart's and so never sees a key the chart has since added
(`sozu.timeouts.connect` included); use `--reset-then-reuse-values`, as
[above](#sōzu-is-drained-on-shutdown-and-the-grace-period-grows-to-40s).
**On Sōzu 2.2.x, a backend that legitimately takes longer than 2 seconds to
accept a connection — a cold runtime, a saturated accept queue — starts being
answered `504` where it previously waited.**

The change is there so that a backend which has gone *silent* fails inside the
proxy, where it is answered and logged, instead of consuming the caller's
entire budget and being recorded on the client as an unexplained hang. On 2.2.x
it buys attribution, not recovery: a connect that times out is logged as
`backend_timeout`, counted in `http.status.504`, and gets no retry and no
failover.

From Sōzu 2.3.0 it buys recovery too, on the HTTP and HTTPS listeners: a
connect that times out is a failed connect, the backend enters back-off, and
the request moves to a backend it has not tried; `503` answers a request whose
every attempt failed. The 2 seconds are now what each attempt waits before
failing over, what each back-off re-trial of a dead backend costs, and what
re-pins a sticky client — see [Sōzu 2.3.2](#sōzu-232). The reason for 2 rather
than 1 is unchanged: Linux retransmits a lost SYN at 1 s.

If it cuts off a backend that was merely slow, raise it:

```yaml
sozu:
  timeouts:
    connect: 5   # or null, to return to Sōzu's own default of 3
```

This reaches the HTTP and HTTPS listeners only. Layer-4 listeners are created by
the controller over the command socket and keep their own fixed budgets, so a
TCPRoute or UDPRoute is unaffected either way — and on 2.3.2 too, a TCPRoute
connect that times out still closes the session with no failover.

---

## The data plane moved from one replica to three, one per node

`replicaCount` was `1`. It is now `3`, spread hard across nodes
(`kubernetes.io/hostname`, `maxSkew: 1`, `whenUnsatisfiable: DoNotSchedule`), and
a PodDisruptionBudget with `maxUnavailable: 1` is rendered alongside — it appears
only above one replica, since over a single Pod a budget can only block every
disruption or permit every one of them.

A single replica has no one to fail over to: while its Pod is rescheduled the
Service holds no endpoint at all, so callers get a connection error rather than a
slower answer. A cluster upgrade triggers exactly that, by design, because the
platform drains and replaces every worker in turn.

Spreading hard rather than preferring is what makes that hold: a preference
measurably does not — a soft constraint put two replicas on one node and left
another empty on every rollout of a three-node cluster, and nothing moves a
running Pod afterwards.

**An earlier version of this note claimed that with fewer nodes than replicas the
surplus Pods stay `Pending` and `helm upgrade --wait` never completes. That is
wrong.** `maxSkew: 1` permits an uneven-by-one distribution; it does not demand a
node per Pod. Tested directly: five replicas on a four-node cluster all reached
`Running`, spread 2,1,1,1. A Pod that does stay `Pending` under this constraint is
short of a node for another reason — taints, resources — not short of skew.

Lowering the count on a smaller cluster is still reasonable, for capacity rather
than for schedulability:

```yaml
replicaCount: 2
```

or keep the count and relax the placement, accepting that replicas may share a
node — which is the wrong shape behind `externalTrafficPolicy: Local`, where a
Pod on every node is what matters rather than a count of Pods:

```yaml
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
```

Both defaults rely on `matchLabelKeys`, honoured from Kubernetes 1.27 and a hard
rejection under strict field validation before it, so the chart now declares
`kubeVersion: ">=1.27.0-0"` rather than leaving that to be discovered.

This buys survival of a Pod or a node going away. It does **not** make a cluster
upgrade lossless on its own — see the reconcile-loop limitation in
[CLAUDE.md](../CLAUDE.md): a controller whose watches stop delivering keeps
serving the routes it last saw, and every replica is then equally stale.

---

## Watch blindness is bounded by default

`controller.watchTimeoutSecs` now defaults to `60`, where the controller
previously inherited kube-rs's own bound. A `helm upgrade` with unchanged values
therefore rolls the Pod and changes how the controller talks to the apiserver.

**What it fixes.** A control plane replaced underneath the controller does not
close the connections its watches ride on: nothing errors, no event arrives, and
the reflectors stop advancing while every reconcile keeps *succeeding* against a
frozen cache. Sōzu then serves routes for backends that moved minutes ago, with
`reconcile_failures_total` at `0` and `/readyz` green. Measured across two
upgrades of a managed cluster, with Sōzu 2.2.1, **17.1% and 14.7% of fresh HTTP
requests through the gateway failed** for the duration of node replacement —
every one an HTTP `504` from Sōzu against a withdrawn backend, while bare TCP,
DNS and the application's own Service stayed clean. With this setting the same
two upgrades measured `0.000%`. Sōzu 2.3 fails a timed-out connect over to
another backend, so the same blindness is expected to show there as failover
latency bursts and `503`s rather than these `504`s; that is not measured, and
the setting stays required either way.

**What it costs.** One watch reconnect per watch per minute. The watcher resumes
from the stored `resourceVersion` rather than re-listing, so a quiet cluster pays
one request per watched kind per minute and nothing else. Set it back to `0` to
restore the previous behaviour:

```yaml
controller:
  watchTimeoutSecs: 0
```

**What the number means.** `60` is `timeoutSeconds` on every watch, which is also
what kube-rs derives its client-side idle timeout from. Both are nominal: cutting
controllers off from the apiserver and timing each to its first logged watch
error gave **336–364s at the default and 117–132s at 60**. Idle expiry logs only
at `DEBUG`, so those figures include the failed reconnect that follows it, and
why they exceed their nominal bound is not established — only that they do. Size
any staleness alert on the measured figures.

**What it does not fix.** This bounds a controller going blind to the
*apiserver* (a stale watch): `/readyz` still stays green in that case — the Pod
serves the last-known routes correctly, it just stops learning — and no
`sozu_gw_controller_*` series moves while it is blind. The setting bounds how
long that lasts; it does not make it visible. (A Sōzu *data-plane* restart is a
different case and now drops `/readyz` until the re-apply, see the shadow entry
above.)

**A restart is not a substitute.** v0.3.0 suggested rolling the gateway once the
control plane had been replaced. That was measured on the next upgrade and does
not work: an arm whose three controllers were restarted on the provider's
control-plane step — confirmed restarted, one Pod at a time — still lost
**16.53%**, indistinguishable from the untreated control, because the fresh
watches attach to an apiserver that is itself about to be withdrawn. Restarting
is only safe once the *new* apiserver is serving, which the provider's step event
does not tell you.

## Downgrading the controller

Upgrades need nothing special. **Downgrades do**, and this is the procedure.

The controller persists its last-applied state to `/run/sozu/shadow.json` as
`{generation, ir}`, where `ir` is a bare `Ir` with no version field. Forward
compatibility is covered — every field
added since is `#[serde(default)]`, and a frozen fixture test
(`crates/controller/tests/fixtures/shadow-v0.2.json`) keeps it that way, so a
new controller reads an old file.

Backwards is the hard direction, and it cannot be fixed by defaulting: an older
build has no variant for an enum value a newer one wrote. `RedirectStatus`
gained `PermanentRedirect`, so a shadow holding it is unreadable to anything
older. serde fails the **whole** parse, not the field, and
[`shadow.rs`](../crates/controller/src/shadow.rs) then starts from an empty
`Ir`.

That is not a crash, which is the problem: diffing from empty produces only
*adds*. Clusters and backends upsert, duplicate frontends are repaired — but
anything in Sōzu that the desired state no longer wants is never removed,
because nothing in the diff asks for it. You get **orphaned routes serving
traffic nobody declared**, and the only trace is one line:

```
WARN persisted shadow is unreadable; will re-apply
```

### The procedure

**Restart the Sōzu container as part of the downgrade.** That is the operative
step: an empty Sōzu makes the controller ignore the persisted shadow outright
(it probes `save_state` first) and re-apply the full desired state against a
clean data plane, so there is nothing left to orphan.

```sh
# roll the whole Pod — both containers share the socket and the volume anyway
kubectl -n sozu-system rollout restart deploy/sozu-gateway
```

Delete `/run/sozu/shadow.json` too if the volume outlives the Pod. It is not
what fixes the orphans, but it stops the next start from parsing a file it
cannot read and logging a warning that no longer means anything.

**Do not** downgrade the controller alone and leave Sōzu running. That is
exactly the case where the shadow is unreadable *and* the data plane is
non-empty, which is the orphan path above.

---

## The `tcp/udp-services` ConfigMaps are removed

**What went away**

| Removed | Replacement |
| ------- | ----------- |
| Helm `l4.tcpServices` / `l4.udpServices` | a `TCPRoute` / `UDPRoute` |
| `--tcp-services-configmap` / `--udp-services-configmap` (`SOZU_GW_TCP_SERVICES`, `SOZU_GW_UDP_SERVICES`) | — |
| the rendered `*-tcp-services` / `*-udp-services` ConfigMaps and their namespaced `Role`/`RoleBinding` | — |

A `helm upgrade` that still sets `l4.tcpServices` or `l4.udpServices` **fails**
rather than silently dropping the routes: unknown values are ignored by Helm, so
the chart checks for them explicitly.

**Read this before upgrading**

Layer-4 routing now requires the **Gateway API CRDs** (v1.6.1 standard channel,
which is where `tcproutes`/`udproutes` live from v1.6 on). On a cluster without
them, the controller runs in Ingress-only mode and there is no way to forward a
TCP or UDP port at all. That is a functional regression, deliberately taken:
the ConfigMap path was cluster-global with no admission control whatsoever —
anyone able to edit the map routed any port to any Service in any namespace,
with no ReferenceGrant and no Gateway to consent — and keeping a second,
weaker way to do the same thing is exactly the kind of quiet approximation this
project refuses.

If you need layer 4 and cannot install the Gateway API CRDs, stay on the
previous release.

**Migrating**

A mapping like

```yaml
l4:
  tcpServices:
    "5432": "demo/postgres:5432"
```

becomes an exposure entry (only Helm can open a port on the Service) plus a
route. The exposure entry was already required in the previous release, so this
half is likely done:

```yaml
# values.yaml
exposure:
  - { name: http,     port: 80,   bind: 8080, protocol: HTTP,  transport: TCP }
  - { name: https,    port: 443,  bind: 8443, protocol: HTTPS, transport: TCP }
  - { name: postgres, port: 5432, bind: 5432, protocol: TCP,   transport: TCP, owner: demo }
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: { name: l4, namespace: demo }
spec:
  gatewayClassName: sozu
  listeners:
    - { name: postgres, protocol: TCP, port: 5432 }
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: { name: postgres, namespace: demo }
spec:
  parentRefs: [{ name: l4, sectionName: postgres }]
  rules:
    - backendRefs: [{ name: postgres, port: 5432 }]
```

The optional `owner` on the exposure entry restricts which namespace's Gateways
may claim that port — the map had no equivalent, so adding it is the point.

Apply the route **before** removing the values: where both named one socket, the
route already won, so there is no gap.

A full worked example is in
[examples/api-gateway/l4-routes.yaml](../examples/api-gateway/l4-routes.yaml).

**Problems that no longer exist**

`InvalidL4Mapping`, `L4PortDuplicate`, `L4PortNotExposed`, `L4PortNotOwned` and
`L4PortClaimedByRoute` are gone with the path that raised them. Their
equivalents on the route path are `PortNotExposed` and `ListenerPortNotOwned`
(on the Gateway listener) and `L4RouteConflict` (on the losing route) — and
unlike a ConfigMap entry, every one of them lands on an object you can
`kubectl describe`.

**Also**

`scripts/e2e-l4.sh` and `examples/ingress/l4-tcp.yaml` are removed;
`scripts/e2e-l4-routes.sh` (`just e2e-l4-routes`) covers layer 4.
