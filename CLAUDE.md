# CLAUDE.md

This is the primary agent instruction file for the repository. It is symlinked to `AGENTS.md`
so other coding agents (e.g. OpenAI Codex) pick up the same guidance.

## What this is

A Kubernetes Ingress controller / API gateway built on the [Sōzu](https://github.com/sozu-proxy/sozu)
reverse proxy. The controller watches Kubernetes objects, compiles them into a neutral
intermediate representation (IR), diffs that IR against the last-applied state, and pushes the
minimal set of mutations to a co-located Sōzu instance over its protobuf **command socket** —
hot, with no proxy restarts. **Phase 1 (Ingress + TLS), Phase 2 (Gateway API) and Phase 3
(HTTPRoute filters: header edits, redirect) are implemented** and validated
end-to-end; see [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md).

## Commands

Tasks are run with [`just`](https://github.com/casey/just) (`just` with no args lists them):

```bash
just build          # cargo build --workspace
just test           # cargo test --workspace (unit + golden/snapshot tests)
just lint           # cargo fmt --check + clippy -D warnings (the CI gate)
just fmt            # cargo fmt (write)
just image          # docker build the controller image
just chart-lint     # helm lint + template (also rbac.allowStatusWrites + metrics/ServiceMonitor)
just e2e            # in-cluster end-to-end (Ingress + TLS) on the current kube-context
just e2e-gateway    # Gateway API + HTTPRoute filters (header/redirect) end-to-end
just e2e-l4-routes  # layer-4 through the Gateway API (TCPRoute + UDPRoute)
just e2e-all        # every e2e suite, sharing one freshly-built image
```

- **`protoc` is required to build** — `sozu-command-lib`'s `build.rs` runs `prost-build`. The
  devcontainer already installs it; on a bare host `apt-get install protobuf-compiler` first.
- **Run a single test:** `cargo test -p sozu-gw-translator <name>` (crates: `sozu-gw-ir`,
  `sozu-gw-translator`, `sozu-gw-builder`, `sozu-gw-agent`, `sozu-gw-controller`).
- **Snapshot tests use [`insta`](https://insta.rs).** Golden snapshots live in
  `crates/*/tests/snapshots/`. After an intentional change to emitted commands, review/accept
  with `cargo insta review` (or `INSTA_UPDATE=always cargo test`). A diff in a `.snap` is a
  behavior change to scrutinize, not a thing to blindly re-bless.
- The [justfile](justfile) is authoritative for task/command names (`image`, `chart-lint`,
  `chart-package`, `e2e`, …); keep the README in sync with it. Override variables before the
  recipe, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.
- The e2e scripts default to an ephemeral `ttl.sh` image (build + push), so no registry
  credentials are needed — just a working kube-context.

## Architecture

```
K8s objects ─▶ reflector caches ─▶ builder ─▶ IR ─▶ translator ─▶ protobuf cmds ─▶ Sōzu socket
```

Six workspace crates, layered so the pure ones can be unit-tested without kube or a socket. The
**purity boundary is load-bearing** — keep `ir`, `builder`, and `translator` free of any socket I/O;
all socket/kube-client I/O lives in `sozu-agent` and `controller`.

| Crate | Role | I/O |
|---|---|---|
| [`crates/ir`](crates/ir) | neutral structs (`Cluster`/`Backend`/`Frontend`/`Certificate`/`Ir`) | none |
| [`crates/gateway-api`](crates/gateway-api) | Gateway API CRD types, kopium-generated (types only) | none |
| [`crates/builder`](crates/builder) | typed Ingress **+ Gateway API** objects → IR (+ `Problem`s/results) | none |
| [`crates/translator`](crates/translator) | pure IR → Sōzu commands, diff vs last-applied | none |
| [`crates/prometheus`](crates/prometheus) | pure `AggregatedMetrics` → Prometheus text exposition | none |
| [`crates/sozu-agent`](crates/sozu-agent) | wrapper around `sozu-command-lib` (socket, ack loop) | **socket** |
| [`crates/controller`](crates/controller) | kube-rs watch/reconcile loop, wires it together | **kube + socket** |

### How the reconcile loop works (`controller/src/main.rs`)

One **singleton, global** reconcile — not per-object. Reflector caches for Ingress, IngressClass,
Service, EndpointSlice, Secret (field-selected to `type=kubernetes.io/tls`) — and, when the
Gateway API CRDs are present, GatewayClass, Gateway, HTTPRoute, ReferenceGrant — each ping a
single mpsc channel on any change (EndpointSlice pings are pre-filtered to services some route
actually references); a debounced
(`SOZU_GW_DEBOUNCE_MS`) reconcile rebuilds the *entire* desired IR from the caches, diffs it
against an in-memory **shadow** (the last successfully-applied `Ir`), and applies only the delta.
A periodic resync (`SOZU_GW_RESYNC_SECS`) re-runs that rebuild, so it heals drift between the
caches and Sōzu — but **not** between the caches and the apiserver. It re-reads the same
reflectors, so a cache that has stopped receiving events re-diffs to "no change": the resync is
then a no-op that still counts as a successful reconcile.

- The shadow advances **to the desired IR only on a fully successful apply**. On failure the
  requests Sōzu acknowledged are **replayed onto the previous applied IR, in order** (the agent's
  `ApplyError::replayable`, fed to `translator::replay_if_exact` with the desired IR the batch was
  diffed towards: an add sets what that IR holds under the translator's key for the object, a
  removal drops what it took out), and the result is **adopted only when it describes exactly the
  routing state a simulation of those requests leaves** — dispatched, the way the agent got them
  acknowledged, onto the `ConfigState` the translator builds from the previous IR, and compared
  with the replayed IR's. That is a proof against the previous shadow, not an observation of Sōzu:
  drift the shadow already missed, the effects of the request the batch stopped on, and
  certificates are outside it. An adopted replay is persisted at once; otherwise the shadow stays
  put, as it did before anything was replayed. A shadow that ignores what landed diffs wrongly from
  then on. A removal it still lists makes an object the desired state wants again diff to nothing:
  the route silently stayed missing on 2.2.1, and on 2.3 an `AddBackend` to the removed, unchanged
  cluster went out without its `AddCluster` and was refused `ClusterNotFound` on every reconcile,
  blocking unrelated changes. An add it misses makes an object the desired state drops again diff
  to nothing: a route re-pointed to another live cluster keeps serving it once deleted (both
  measured, `docs/probes/partial-apply_*`). The request the batch stopped on never counts, since
  its effects are not reliably established (2.2.1 can refuse a `ReplaceCertificate` after dropping
  the old certificate; a lost reply says nothing), and the 60 s apply timeout replays nothing,
  since the count never reaches the controller. **The replayed head also stops before the first
  teardown the agent settled without proof** that the main process dropped the object: only the
  main process's own refusal, or a worker answering `OK`, proves it. When every worker refuses a
  frontend removal, 2.3's main process reverts it in its own state (`should_rollback_fanout`,
  for the changes `ConfigState::rollback_for` inverts: frontend removals, listener and frontend
  additions, listener activation), and a worker it forks later is fed that state; an eviction
  every worker refused leaves the route in the main process the same way. The answer tells
  neither the version (2.2.1 reverts nothing) nor whether the verb has an inverse, so the agent
  cuts at every such teardown, backend, cluster and certificate removals included. A fully
  successful batch still advances the shadow to the desired IR over such a leftover — the
  main/worker divergence is a follow-up. A certificate add is replayed as the desired
  certificate, but Sōzu takes an add of a fingerprint it already holds as a no-op, names and
  material included: over a drift the shadow missed (that fingerprint installed with other names)
  the shadow then lists names Sōzu does not hold, as the unchanged shadow's retried add would leave
  them too. **Some states a head leaves have no IR spelling**, and from a baseline that misdescribes
  one some desired state diffs wrongly whichever IR is picked, so those keep the previous shadow:
  a layer-4 listener held without its frontend (a frontend removal that landed before its
  `RemoveListener`, a listener added without its frontend), and a cluster upserted ahead of the
  removal of its last UDP frontend (the translator derives UDP affinity from a cluster's UDP
  frontends, so the replay would spell back an affinity Sōzu no longer holds, and a desired state
  wanting the frontend back would never re-send it). From the kept shadow a retry of the same
  desired state converges, but one that wants back what the head undid gets nothing for it (the
  listener stays bound, the frontend missing, the cluster without affinity): a kept shadow
  guarantees nothing about other desired states. The 2.3 `ClusterNotFound` wedge needs a
  `RemoveCluster` to have landed under a shadow that still lists the cluster; cluster removals are
  tiered last, after the listener teardown, so a head that holds one has landed everything else of
  its batch but later cluster removals, none of the states above remains, and its replay is
  adopted — unless the head was cut before it or the shadow had already drifted. Re-diffing
  converges from an adopted shadow, which describes what that simulation holds, and from a kept one
  towards the batch's own desired state — NOT because the requests are idempotent
  (frontend/listener `Add*` verbs reject duplicates with `StateError::Exists`), but because
  `AddCluster`/`AddBackend` upsert and the agent tolerates teardowns of objects Sōzu says it no
  longer holds (only those: a teardown refused for any other reason fails the batch) and *repairs*
  duplicate frontend adds (evict whatever holds the key, then re-add; see `sozu-agent`). Both look
  the key up in Sōzu's own frontend list first: the stored entry can belong to a cluster the shadow
  never knew, and Sōzu honours only a removal that names the cluster holding the key (always for
  TCP/UDP, for HTTP/HTTPS since 2.3.0), so the eviction is built from the stored entry, and a
  teardown answered "not found" while the key still routes for another cluster removes that route
  rather than being tolerated. A `ReplaceCertificate` refused because Sōzu holds no certificate at
  that listener (since 2.3.0 the main process drops a listener's certificate entry with its last
  certificate, so a rotation diffed from a shadow that missed the removal is refused "Did not find
  Certificate" on every pass) is applied as an
  `AddCertificate` of the replacement; any other refusal of a replace fails the batch. A failed
  reconcile is **retried without waiting for a watch event**: the liveness tick re-runs pending
  work on its next successful probe, the resync rebuilds anyway, and with both disabled the loop
  nudges itself after a short delay — a quiet cluster must never keep a failed apply pending.
- **Fail-fast philosophy:** if a watch stream ends or caches don't sync within the timeout, the
  process exits so Kubernetes restarts it rather than silently going blind. Never `panic!`.
  **It does not cover a stream that goes silent without ending.** `watcher` retries internally, so
  a watch that stops delivering surfaces as neither an item nor an error and the reflector just
  stops advancing, and no `sozu_gw_controller_*` signal moves. What bounds it is the watch's own
  idle timeout: the binary defaults `--watch-timeout-secs` to 60 and the chart's
  `controller.watchTimeoutSecs: 60` mirrors it (an explicit `0` opts out and is rendered as such;
  ≥ 295 is refused at startup because kube-rs refuses it on every watch start — see
  [docs/UPGRADING.md](docs/UPGRADING.md)), where the kube-rs default leaves it near five
  minutes. Measured across three managed-cluster upgrades on Sōzu 2.2.1, the unbounded default
  cost **19.7%, 17.1% and 14.7%** of fresh HTTP requests for the duration of node replacement —
  every failure an HTTP 504 from Sōzu against a withdrawn backend, while bare TCP, DNS and the
  application's own Service stayed clean, and `reconcile_failures_total` stayed 0 with `/readyz`
  green throughout. At 60 the same upgrades measured **0.000%**. On 2.3 a timed-out HTTP connect
  fails over instead, so the same blindness should surface as latency bursts and 503s — inferred,
  not measured; the bound stays required.

  Two traps around this, both measured rather than reasoned:
  **restarting the controller is not a reliable cure** — an arm restarted on the provider's
  control-plane-replaced event, confirmed Pod by Pod, still lost 16.53%, because the fresh watches
  attach to an apiserver that is itself about to be withdrawn; and **blindness alone is free** —
  four controllers cut off from the apiserver for eight minutes with no backend change lost
  nothing at all. It costs traffic only when the world moves while they cannot see it, which is
  why a quiet cluster never shows this and an upgrade always does.
- The shadow is **persisted** to the shared volume (`--shadow-file`, default `/run/sozu/shadow.json`)
  the moment a socket apply succeeds — before the status/Event apiserver calls that follow, which
  must never hold the baseline back — and reloaded at startup, so restarting *only* the controller
  resumes from the real baseline and still prunes orphans. The file carries the **restart
  generation** of the Sōzu it was applied to (command-socket identity + live worker PIDs) and is
  resumed **only if the Sōzu found at startup answers with the same socket identity**: a Sōzu that
  restarted while the controller was down recreated its socket, so the file is ignored and the full
  state re-applied. (An emptiness probe via `save_state` cannot do this: with the static listeners
  a fresh Sōzu already dumps four records, so it never reads "empty".) Mid-life, a restart under a
  live controller is detected the same way (checked on every resync tick, pending reconnect, and
  post-apply reconnect); a changed **socket** resets the shadow, and the file with it, so the next
  reconcile re-applies everything, and **`/readyz` drops for that window** so the Service stops
  sending traffic to a Pod whose Sōzu holds no routes; it re-latches when the re-apply succeeds. An
  idle socket never reconnects, so a **liveness tick** (`--sozu-probe-secs`, default 2, chart
  `controller.sozuProbeSecs`) runs that same generation check on a timer — one `Status` round-trip,
  nothing rebuilt unless it finds a restart — bounding how long a Sōzu that restarted alone stays in
  the Service before the reset fires (without it, only the next watch event or resync notices, up to
  `SOZU_GW_RESYNC_SECS`, never at 0). A
  changed worker set on the *same* socket does **not** reset:
  the main process re-feeds its state to a respawned worker, and diffing `empty → desired` would
  emit only adds, never removing what left the desired state meanwhile. Worker PIDs alone could
  never detect a restart anyway: a restarted container can reuse the same PID set. Socket
  identity must describe the connection that answered the worker probe, not a later pathname
  lookup that could already refer to a replacement socket. Its ctime part is load-bearing: on a
  disk-backed filesystem the new socket can reuse the old inode (measured on overlayfs; the
  chart's tmpfs `emptyDir` gave a fresh one). On 2.3.x a bare SIGTERM soft-stops Sōzu: until the
  main exits, `Status` still answers and frontend mutations fail "found no listener" (removals
  tolerated, adds retried; measured); the reset then fires on the new socket. A fan-out that no
  worker acknowledged within `worker_timeout`, but that the workers then applied late, leaves the
  main process and the workers apart: an add the main rolled back that the workers serve (every
  re-add then fails "Could not add route"), or, since 2.3.1, a removal the main restored that no
  worker serves — a "ghost" a re-forked worker resurrects. Removing its cluster drops a ghost
  (cascade), and `sozu upgrade --worker` re-forks every worker from the main state without
  changing socket identity, after which the retry converges (measured on 2.3.2; PROTOCOL.md §6).
- **The shadow file is `{generation, ir}` and the `ir` half is a bare `Ir` with no version field**, so every new `Ir` field needs `#[serde(default)]`
  — enforced by a frozen fixture (`controller/tests/fixtures/shadow-v0.2.json`), not by convention.
  The reverse direction cannot be defaulted: an older build has no variant for an enum value a
  newer one wrote, serde fails the *whole* parse, and diffing from the resulting empty `Ir` emits
  only adds — so orphans in Sōzu are never pruned. **Downgrading therefore requires restarting
  Sōzu**, see [docs/UPGRADING.md](docs/UPGRADING.md). Adding an `ir` enum variant is a
  downgrade-breaking change and should say so in its commit.

### Translator diff strategy — the subtle part

The translator deliberately uses **two different diff strategies**, and changes here are easy to
get wrong:

- **Routing graph** (clusters/backends/frontends): reuse Sōzu's own `ConfigState::diff` so the
  semantics match the data plane exactly. Certificates are kept *out* of this path.
- **Certificates**: diffed by hand, keyed by `(listener, fingerprint)` — Sōzu's own cert identity.
  This emits `ReplaceCertificate` for zero-gap rotation and, on an unchanged leaf, for a change of
  its names or of its served chain (DER, without copies of the leaf), which 2.3.2 applies in place;
  of several Secrets sharing a leaf, the builder keeps the fullest chain (then the smallest
  material), so read order never flips it.
  Sōzu 2.3's own certificate diff would instead move a cert with a non-atomic Remove + Add
  (a gap), and turn an order-only or same-fingerprint change into Remove(X) + Add(X), which the
  tiering below (adds before removes) would turn into a lost certificate. (It also kept clear of a
  `debug_assert` in `sozu-command-lib` 2.1.0–2.2.1 that fired when `ConfigState::diff` removed the
  last cert at a listener; 2.3 dropped it.)

All output is reordered into **dependency-safe tiers** (`canonicalize` / `tier()`): adds go
clusters → backends → certificates → frontends; removes in reverse, except that cluster removals
come **last**, after the layer-4 listener teardown (nothing depends on a cluster going, and on 2.3
its cascade then only meets what is already removed; see the reconcile-loop bullet for why).
Frontend *removes* are tiered **before** frontend *adds*: Sōzu keys a route by
`address;hostname;path[;method]` (*not* `cluster_id`), so re-pointing a host+path at a different
cluster is a `Remove`+`Add` on the same route key, and adding first would be rejected as a
duplicate (`StateError::Exists`) — there is still no atomic frontend replace in 2.3.2. Since 2.3.0
`RemoveCluster` cascades to the cluster's frontends and backends (c2cad338) and
`ConfigState::diff` relies on it, emitting no removal for what the cascade takes; tiered after the
adds, a moved route would then be added while its old frontend still held the key. `reconcile`
therefore re-expands each `RemoveCluster` with `ConfigState::cluster_removal_requests`
(`expand_cluster_removals`), so batches keep their 2.2 shape — for a desired state whose every
backend and frontend names a declared cluster, which `reconcile` enforces
(`TranslatorError::UnknownCluster`; upstream's diff would re-add a frontend naming a removed
cluster after the cascade, the tiers before it) — and `RemoveCluster` comes after them all.
Measured on the test cluster against 2.3.2 without it: an Ingress switched to another Service
failed **every** reconcile (21 failures in ~40 s); with it, none. The re-expanded batches also
apply cleanly to a 2.2.1 data plane (measured). A replacement cert lands before the old is
removed (no TLS gap), and within the certificate tier every name that moves is given before it is
taken (`CertRank` + `transfer_order`, not the JSON sort): adds, rotations and replaces that only
gain names first, replaces that only drop names last, and the replaces that do both in a
deterministic topological order, recipient before donor, ties by the JSON key — so a chain of
transfers (`{x}`→`{y}` on one certificate, `{y}`→`{z}` on another) runs recipient first whatever
the PEM texts. The other way round the name falls back to Sōzu's default certificate in between
(measured, [docs/probes/](docs/probes/) `sni-ownership-race_*`). Only transfers that form a
**cycle** (certificates swapping names) cannot all go first: the replaces on a cycle keep the JSON
order among themselves, so one name on it can still fall back for that moment. This also makes
the HashSet-ordered routing diff deterministic for golden snapshots. In debug builds
`ConfigState::diff` replays its own output with an order-sensitive check that panics when a
cluster gains a TCP frontend sorting before one it already holds (2.2.1 and 2.3.2) — keep that
shape out of tests.

### Gateway API (Phase 2)

`crates/gateway-api` holds **kopium-generated** CRD types (v1.6.1 standard channel,
`--schema=disabled`; regenerate per its README — do not hand-edit). The builder's
[`gateway` module](crates/builder/src/gateway.rs) maps GatewayClass/Gateway/HTTPRoute through the
**same** Service→pod-IP resolver and into the **same** IR as Ingress (a route and an Ingress to one
Service share a cluster). `allowedRoutes.namespaces.from: Selector` is evaluated against a Namespace label
index (a cluster-wide Namespace watch, labels only); a selector this build *cannot* evaluate still
fails closed and is reported. Gateway listeners map to the static listeners by protocol and must
declare the **advertised** ports (default `80`/`443`, `--gateway-http(s)-port` — the Service's
client-facing ports, wired by the chart; a mismatch is rejected with `PortUnavailable`); cross-ns
refs are gated on ReferenceGrant. Anything Sōzu can't represent (weighted multi-backend split,
query matches, TLS passthrough) is reported as a `Problem` and skipped, never approximated. The
one backend kept as it was is a `kubernetes.io/wss` port: reported, but still programmed (see
Conventions), because skipping it would hand its requests to another rule.

**Header matches (Sōzu ≥ 2.3).** HTTPRoute `matches[].headers` become frontend header predicates
(`ir::Frontend::header_matches`, kept in Sōzu's canonical order because `HttpFrontend` equality
is order-sensitive), sent on verbs 66-69 through `RequestType::add_http_frontend` & co — never a
hand-picked verb: a pre-2.3 worker given the historical one installs the route unguarded. Sōzu
ranks every frontend whose predicates hold above every frontend without any, whatever the path or
method, so the builder's `lift_header_matches` copies each plain route that outranks a header
match under Gateway API precedence (Exact path, longer prefix, method; never a regex path) above
it, taking the header match's method where the plain route names none (`lifted_copy`: a copy must
never match a request its header match cannot), at most 64 *added* copies per hostname
(`HeaderMatchPrecedenceLimit` beyond). That budget bounds the route table, **not the work**: the
admission is pairwise and replans after each skip, cubic in the header matches of one hostname,
and header matches that need no copy never touch it. What bounds it is
`MAX_HEADER_MATCHES_PER_HOSTNAME` (64, `HeaderMatchLimit` beyond, oldest route first so input
order never decides), per-(listener, hostname) planning, and an admission that re-checks only
what the last skip changed (debug builds cross-check it against a full scan on small tiers).
Past the cap one tenant makes one report per skipped match, so reports reach their Ingress or
route through a `(namespace, name)` index: a scan per report was quadratic in the routes again.
Don't raise the cap or merge the groups without re-running the adversarial tests. Inside the
predicated class Sōzu's selection partly follows the order it holds frontends in — and that order
differs between a fresh batch, an incremental add and a re-forked worker's key-order replay (#80,
no ordering workaround). So **every pair** of predicated frontends (header matches and copies, a
copy ranking as the plain route it copies) that one request can meet, to different backends or
filters, must be ordered the Gateway API's way by `unsettled`'s two order-free outcomes (a regex
naming the method over any rule that is not one; a regex that cannot match `/` over the root
prefix),
or the lower header match is skipped (`HeaderMatchOutranksRoute` / `HeaderMatchOverlap`) — on a
hostname-less route (first-match list) every such pair is unsettled. This refuses deterministic
wrongs too: an Exact path beside a prefix with a method. Two copies are weighed against each
other like any pair: they follow the plain routes' rules but in an insertion order of their own
(a header-matched root above plain Exact `/foo` and prefix `/foo` is skipped), so header matches
never leave more to Sōzu's order than the same plain routes already do (#80). "Can meet" is
conservative (`can_meet`: only disjoint paths, different methods or incompatible values on one
header keep two apart), so don't loosen it without a measurement. A copy is dropped for a
frontend holding its Sōzu key only when that frontend is its very match (`same_match`); a regex
path can spell any key, and then the header match is skipped (`HeaderMatchCopyKeyTaken`). The
tests replay every order through a model of Sōzu's selection, including a randomized property
test over overlapping plain routes too. Headers
Sōzu sets before it routes (`SozuRequestHeaders`, measured) and whatever
`validate_header_predicate` refuses are reported, not programmed; the two config-dependent ones
reach the controller as flags the chart renders from the same values as Sōzu's config. The
agent's settlements and evictions, and the failed-batch replay with its simulation
(`translator::replay_if_exact`), treat verbs 66-69 as their plain siblings, under a key that
carries the predicates. See [PROTOCOL.md §14](PROTOCOL.md).

**Phase 3 — HTTPRoute filters.** `RequestHeaderModifier`/`ResponseHeaderModifier` and
`RequestRedirect` (scheme, status, and hostname/path/port targets since 52cf36e) compile into
per-frontend `ir::FrontendFilters`, which the translator maps onto Sōzu's frontend fields. A
non-empty header value appends; an empty value deletes. Gateway `set` therefore emits a deletion
followed by an append on the same frontend,
while `add` appends and `remove` deletes. Empty Gateway `set`/`add` values are refused with
`Accepted=False`, `UnsupportedValue`, because they would otherwise become deletions. Unsupported
sub-fields (`RequestMirror`, for example) are reported, never half-applied. A `RequestRedirect`
rule has no `backendRef` (the API forbids it), so it becomes a **cluster-less frontend** — hence `ir::Frontend::cluster_id` is
`Option<String>`. **`URLRewrite` is reported, not wired** — a *choice* for its path, an upstream
bug for its hostname. Measured on Sōzu 2.2.0 and re-measured unchanged on 2.2.1 and 2.3.2
([PROTOCOL.md §13](PROTOCOL.md), [docs/E2E-RESULTS.md §5c](docs/E2E-RESULTS.md)), which also
retired an earlier `408` result taken against 2.1.0. Two measured conditions gate any path
wiring: a literal `$` in a rewrite value makes Sōzu **reject the frontend** (`$$` is no escape,
and translation is all-or-nothing, so one such route fails every reconcile), and a literal
`rewrite_path` **drops the query string** that `ReplaceFullPath` keeps — on a redirect too, so
the wired `path.replaceFullPath` redirect drops it today. `$PATH[n]` indexes the path rule's
capture groups, and today's `ir::PathMatch::sozu_rule` regexes have none, so `$PATH[1]` gets the
frontend rejected. A capturing spelling of the non-root prefix regex,
`^<esc>(?:(/)((?-u:[^?\n]*)))?((?:\?(?-u:.*))?)$` (the same language), yields boundary,
remainder and query: it keeps the query and makes `ReplacePrefixMatch` expressible (measured),
but is not wired. The root `/` must stay Sōzu's native `Prefix "/"`: a root regex competes with
every other regex and the last added wins (measured). `URLRewrite.hostname` is blocked:
`rewrite_host` sends **two** `Host:` lines to an HTTP/1 backend, which a Go backend answers `400`
(measured on both versions). The translator already maps `ir::Rewrite`, so the builder side is
the only piece missing.

The CRDs are **optional**, in two tiers: GatewayClass/Gateway/HTTPRoute/ReferenceGrant are
*required* (any one missing ⇒ Ingress-only), TCPRoute/UDPRoute are *optional* (missing ⇒ no
layer-4 routing, everything else unaffected). `crd_served` reads **only a 404** as absent, so the
RBAC grant must cover every probed kind: the v1.6.1 standard channel ships tcproutes/udproutes, and
an ungranted probe answers **403**, which is fatal by design. Status
(`Accepted`/`Programmed`/`ResolvedRefs`) is written by [`controller/src/status.rs`](crates/controller/src/status.rs),
which is **loop-safe** — it reuses `lastTransitionTime` for unchanged conditions and skips no-op
patches, so the controller's own status writes never re-trigger it. Status writes are best-effort.
The route writer is **generic over the route kind** (kopium emits one status triple per kind with no
trait in common, so `RouteParents` is declared controller-side and implemented per kind) and keys a
`status.parents[]` entry on the **whole** parentRef, `sectionName` and `port` included: one route may
name a Gateway once per listener, and matching on `(name, namespace)` alone collapses those entries
into one whose `lastTransitionTime` then moves on every pass.
`Problem`s also surface to users: as the detail in `False` condition messages, and as Warning
**Events** on the owning Ingress/Gateway/route ([`controller/src/events.rs`](crates/controller/src/events.rs),
diffed against the previous pass so resyncs never flood etcd).

**Conformance is a documented partial, by design.** On the v1.6.1 suite, unconditioned, the
GATEWAY-HTTP profile scores **20/37 core, 1/3 extended** — full run log, per-test attribution and
reproduction in [docs/E2E-RESULTS.md](docs/E2E-RESULTS.md) §6, reports in
[docs/conformance/](docs/conformance/) (immutable, one file per run). Rows 2–4 of that log predate
the `Selector` implementation, when the suite **aborted in setup** because
`NamespacesMustBeReady` demands every base Gateway be `Programmed: True` and one of them uses
`from: Selector`; those rows are conditioned and must never be quoted bare. The profile **cannot
fully pass** on Sōzu (no per-request weighted splits, no query matching, no HTTP 500,
header matches Sōzu can only order by insertion are refused — HTTPRouteHeaderMatching loses three
of its matches), so don't chase
the "Conformant" badge — and don't read the recorded failures as regressions.
`GatewayClass.status.supportedFeatures` is published **empty** on purpose: an entry goes in only
when a recorded run shows its tests passing. [docs/features.md](docs/features.md) is the
user-facing support matrix (supported / planned / not supported); keep it in sync when support
changes.

### Conventions that matter

- **HTTP/HTTPS listeners are NOT modelled in the IR.** They are declared
  statically in Sōzu's `config.toml` ([deploy/sozu/config.toml](deploy/sozu/config.toml)) and
  activated at boot; their addresses come from the chart's `exposure` table. **L4 (TCP/UDP)
  listeners are the exception**: their ports are user-defined, so the IR carries
  `ir::L4Frontend`s and the translator adds + activates the listeners dynamically over the socket —
  `ConfigState::diff` emits `Add{Tcp,Udp}Listener` + `ActivateListener` (and the reverse on removal)
  for free.
- **Layer-4 routes are `TCPRoute`/`UDPRoute` on a `protocol: TCP`/`UDP` Gateway listener, and
  conflicts are settled in the builder.** (The cluster-global `tcp/udp-services` ConfigMaps are
  gone — see [docs/UPGRADING.md](docs/UPGRADING.md).) Two routes contesting a socket are settled by
  oldest `creationTimestamp` **then**
  `namespace/name` — the timestamp has one-second granularity, so without the second key the winner
  would follow cache iteration order and flip between reconciles. The translator's
  `check_l4_conflicts` stays as a net, but it must never be what settles this: it returns an error
  that `reconcile` propagates with `?`, so one tenant's second route would fail the whole reconcile,
  HTTP included.
- **A layer-4 port can never be 443** — the Service already publishes `443/TCP` for `https` and a
  Service cannot expose one `(port, protocol)` twice — and no bind may be privileged (uid 1000,
  all capabilities dropped). Both are `fail`ed by the chart's `validateExposure` helper rather than
  left to a raw apiserver rejection.
- **Backends are pod IP:port resolved from EndpointSlices — never the Service ClusterIP.** When a
  Service has multiple ports, match the EndpointSlice port by name; only fall back to the sole port
  when there is exactly one (don't guess `first()`), and only when it has no name and the Service
  port's protocol (`slice_port`). The EndpointSlice controller writes an unnamed Service port as
  `""`, so `""` and no name are one name; and it gives pods resolving only one port's named
  `targetPort` a slice carrying that port alone, which a name-blind fallback would feed to every
  port of the Service.
- **A backend port's `appProtocol` is read on HTTP routes only** (GEP-1911), at the Ingress and
  HTTPRoute call sites — never inside the shared `add_service_route`, which TCPRoute/UDPRoute use
  (a layer-4 route to a TLS port is passthrough). An HTTP route's port *number* resolves to the
  Service's TCP entry when the number is also exposed over UDP (`PortRef::HttpNumber`); a layer-4
  route still takes the first entry. Each entry is its own cluster — the first carrying a number
  keeps `namespace.service.port`, a later twin gets its protocol appended (`….53.tcp`) — so twins
  never pool their pod ports. `kubernetes.io/h2c` turns `ir::Cluster::http2` on, and the
  translator emits it as `Some(true)` or not at all: an `AddCluster` without it resets Sōzu to
  HTTP/1.1, measured. `kubernetes.io/wss` is refused **in status only**: an HTTPRoute reads
  `ResolvedRefs: False`/`UnsupportedProtocol`, it and an Ingress get a Warning Event, but the
  rule is **still programmed as before** (cleartext HTTP/1.1) — dropping its match would let
  another rule answer it (`/ws` falling to `/`). No empty-cluster `503` either: every other invalid
  backendRef drops its match today, and #78 tracks a native error answer. Every other value keeps
  HTTP/1.1.
  Sōzu 2.2.1 cannot serve HTTP/1.1 clients of an h2c cluster (`504`).
- **`pathType: Prefix` is element-boundary matching, not a string prefix.** `/foo` covers `/foo`,
  `/foo?q=1` and `/foo/bar` but never `/foobar`, so the translator compiles a non-root prefix to an
  regex — Sōzu's own `Prefix` rule is a raw `starts_with`. Two measured facts drive the
  pattern: Sōzu matches path rules against the request target with the **query string attached**,
  and 2.2.1 does **not** anchor regexes — while 2.3.0 and later wrap every `Regex` rule in
  `\A(?:…)\z` (upstream 47eb07c, BREAKING). **Every generated pattern is therefore full-span:**
  `^<escaped>(?:[/?](?-u:.*))?$` for a prefix, so it reads the same under both routers, and the
  `(?-u:.*)` tail is bytes, not UTF-8 characters, because Sōzu compiles with `regex::bytes` whose
  Unicode `.` skips a byte that is not valid UTF-8 (the `ir` crate's
  `sozu_rules_match_the_same_targets_unanchored_and_anchored` pins both properties on Sōzu's own
  `regex` version). The root `/` stays a plain prefix. The builder canonicalises
  `/foo/` to `/foo` first, so one path has one IR spelling. **`pathType: Exact` is a full-span
  regex too** (`^<escaped>(?:\?(?-u:.*))?$`, trailing slash kept literal), never Sōzu's `Equals`: measured
  on 2.2.1, `Equals` misses a query-bearing target and cannot be removed once added (the worker's
  rule equality has no `Equals` arm), so a removed `Exact` route kept serving. 2.3.0 adds the arm
  (795b95d) but `Equals` still misses `/p?x`, so `Exact` stays a regex. The compilation
  lives in `ir::PathMatch::sozu_rule` and both the builder's collision key and the translator use
  it, so two spellings of one Sōzu rule are one route everywhere. That collision key is
  `ir::Frontend::sozu_route_key`, Sōzu's own unescaped `address;hostname;rule[;method]` string,
  plus one `;H{kind}:{name:?}={value:?}` per header predicate since 2.3
  (pinned to `RequestHttpFrontend::to_string()` by a translator test), never a tuple: a regex
  `/x;GET` and `/x` + `GET` differ as tuples but are one key — so are a regex spelling a
  predicate suffix and the predicated route — and one unarbitrated clash fails every reconcile.
- A frontend becomes HTTPS-enabled only if a TLS host with a *successfully loaded* cert covers it.
  Wildcard TLS hosts (`*.example.com`) cover exactly one extra label.
- **An SNI name at a listener belongs to one claimant, settled in the builder.** Sōzu serves a
  name from whichever of its certificates expires last, so any tenant could take another's
  hostname with a far-future certificate. The objects declaring the name with a certificate
  (Ingress `tls[].hosts`, a Gateway listener's `hostname` or inferred names) are arbitrated by the
  route keys' `OwnerClaim` (oldest `creationTimestamp`, then the joined `namespace/name`), **per
  name, never per fingerprint** — a newer tenant sharing an older one's leaf borrows no seniority —
  and copies of the winner's own certificate are no rival. The age is the *object's*: a tenant
  that edits an older object of its own to add a host wins it, as with route keys, so this closes
  the takeover only against tenants whose objects are all newer. Losers get one
  `CertificateNameClaimed` per certificate, listing the names (a tenant sets their number; keep
  this path linear, see the 100,000-name test) in a message held to 900 bytes, every piece clipped
  on a character boundary — an Event note over 1 kB is refused, and `events.rs` never retries one.
  Arbitration settles names only: the material a leaf is served with is chosen afterwards among
  **every** entry carrying it, name-less ones included (fullest chain, then smallest material), so
  a holder stripped of its names still lends its chain and losing a name never downgrades another
  holder's chain. A leaf (listener + fingerprint) left with no name at all is dropped, **never sent
  with `names: []`**, which Sōzu reads as "infer the SANs". Stripping a name from a
  loaded certificate is a same-fingerprint `ReplaceCertificate`, which only Sōzu ≥ 2.3.2 applies.
  Wildcard-vs-exact overlap is not arbitrated.
- **Tenant input Sōzu would reject is refused in the builder, with the calls Sōzu makes.** A
  `tls.key` is parsed and loaded the way Sōzu loads it (`rustls-pki-types` + the ring provider)
  and paired with the leaf exactly as Sōzu 2.3.2 pairs it (`tls::build_validated_certificate`):
  `rustls::sign::CertifiedKey::keys_match` on the leaf DER from
  `sozu_command_lib::certificate::parse_pem`, accepting only `Ok` or `InconsistentKeys::Unknown`.
  Any leaf rustls-webpki cannot parse as an end-entity cert, or whose SubjectPublicKeyInfo differs
  byte-wise from the one ring derives from the key, is therefore refused — measured refused by
  2.3.2 and served by 2.2.1: X509v1, a compressed EC point, an RSA SPKI with absent instead of
  `NULL` parameters, an outer/TBS signatureAlgorithm mismatch, an unknown critical extension, a
  leaf over 64 KiB of DER. Its hosts lose their HTTPS frontend and HTTP→HTTPS redirect; plain
  HTTP keeps routing. **Every path rule that compiles to a
  regex** — a user `ImplementationSpecific`/`RegularExpression` pattern, and the anchored regex a
  non-root `Prefix` or an `Exact` becomes — is compiled with `regex::bytes::Regex::new` on the
  `regex` version Sōzu 2.3.2 builds against, **twice**, bare and wrapped in `\A(?:…)\z`, as the
  2.3.2 main process does before dispatch: the crate refuses a compiled program past 10 MiB (a
  literal path of a few hundred kilobytes, which Kubernetes accepts, reaches it), and the wrapper
  alone can fail (`/a(?x)#comment`, nesting 248–249 deep); and **every frontend hostname** goes
  through `idna::domain_to_ascii` on the `idna` version and Unicode data Sōzu 2.3.2 builds
  against (`admit_hostname`; parity re-measured on 2.3.2),
  because Kubernetes checks only the RFC 1123 grammar and an A-label such as
  `xn--a.example.com` fails IDNA. It is checked on the name actually emitted, not per label:
  in a right-to-left domain `*.xn--4gbrim.example.com` fails while `a.xn--4gbrim.example.com`
  parses. Certificate names skip IDNA in Sōzu and are not checked. Measured (2026-09-21,
  hostnames 2026-09-23): any of these inputs unvalidated makes Sōzu reject the request, and since
  translation is all-or-nothing that fails every reconcile of the shared instance until the
  object is removed. The builder is the only place these strings enter the IR; the translator
  does not re-validate. Those crate versions, as the builder links them (the lock's edges from
  its own `regex`, `idna`, `rustls` and `sozu-command-lib`, so an unrelated second copy is not
  drift), are held by the lock and checked against Sōzu's by `builder/tests/sozu_lock_pins.rs`:
  regex, idna/ICU and the x509-parser/der-parser/asn1-rs trio are dependabot-ignored and move only
  with Sōzu; rustls, rustls-webpki, rustls-pki-types and ring are pinned but not ignored (they are
  the controller's own TLS stack too), so an update turns that test red and waits for the Sōzu
  release that ships it.
- **Health checks are TCP-connect only, opt-in, and admitted by the builder.**
  `ir::Cluster::health_check` comes from the `sozu.io/health-check*` Service annotations or the
  controller default (`--default-health-check`, chart `sozu.healthCheck`, off). The builder
  validates each with Sōzu's own `validate_health_check_config` (`health_check_refusal`), since
  `ConfigState::add_cluster` refuses an invalid one and translation is all-or-nothing, and an
  unusable one is reported (`InvalidHealthCheck`) and programs no check — never the default.
  The translator spells out `mode: TCP` and `uri: "/"` (the proto default is HTTP with an empty
  `uri`, which Sōzu refuses) and never attaches a check to a UDP cluster. The builder settles
  checks only after route-key and L4 socket arbitration (`settle_health_checks`), since a cluster
  is created the moment a backend resolves: a check stays only on a cluster a programmed HTTP(S)
  frontend proxies to or a TCP frontend routes to, never on one a UDP frontend names, and a Service
  port shared with a UDPRoute is reported (`HealthCheckNotApplied`) on every owner that loses it. A
  check-only change is one `AddCluster` upsert (measured, `docs/probes/health-check_*`).
- **Metrics are pulled, not pushed.** Sōzu has no native `/metrics`; the controller serves one
  (`--metrics-listen`, off when the flag is absent; the chart sets it by default via
  `metrics.enabled`) by issuing a `QueryMetrics` over the command
  socket on each scrape — **`no_clusters` unless `--metrics-per-cluster`** (chart
  `metrics.perCluster`), because a worker whose per-cluster answer exceeds
  `max_command_buffer_size` requeues it forever and stops serving (measured on 2.2.1 and still
  on 2.3.2, whose message is about 12% larger, [docs/probes/](docs/probes/)) —
  and rendering the returned `AggregatedMetrics` with the pure
  [`prometheus` crate](crates/prometheus), prefixed by the controller's own health signals
  (`sozu_gw_controller_*`, incl. the last-successful-reconcile timestamp — the staleness alert).
  It is best-effort and orthogonal to routing: a socket
  error returns `503`, never a panic. Sōzu's histogram buckets are already cumulative (`le`), so they
  map straight onto Prometheus `_bucket`; `Percentiles` become a `summary` (Sōzu only max-merges them
  across workers — the companion `*_histogram` is the accurate aggregate).
- The Sōzu command socket takes a **bare length-prefixed `Request`**; replies come back as
  `Processing` → `Ok`/`Failure`, so every send loops until a terminal status — and the first reply
  may already be terminal (a refusal by the main process, or 2.3's `Ok` "nothing to remove").
  This protocol is
  **verified against a live Sōzu**, documented in [PROTOCOL.md](PROTOCOL.md) (the source of truth
  for the translator). Never reimplement the wire
  format or invent protobuf fields — reuse the crate's types and conversions
  (e.g. `addr.into()`, never hand-pack an address).

### Version pins (verified, do not bump casually)

`sozu-command-lib` **2.3.2** (LGPL-3.0, pinned exactly) against Sōzu **2.3.2**, `kube` **4**, `k8s-openapi`
**0.28** with feature `v1_36` (the e2e cluster's version). Gateway API types are generated from the
**v1.6.1** standard-channel CRDs with `kopium` **0.24** (the published `gateway-api` crate targets
`kube` 3 / `k8s-openapi` 0.27, so it can't be used here). Workspace is edition 2021,
rust-version 1.93.1 — a floor the dependency tree sets, not our own code.

The library and data-plane image are aligned at 2.3.2 and move together (an `image.sozu.tag`
below 2.3.0 is documented unsupported, not refused). The ICU data, rustls 0.23.46 and
rustls-pki-types 1.15.1 are held at Sōzu 2.3.2's versions by the lock (see the tenant-input
bullet). The library redacts certificate and key material from `Debug`. The proto adds fields we
leave unset (listener `interface`, `AddBackend.unix_path`, `Cluster` fields 17–23) and verbs
60–75 we never send, but for frontend `header_predicates` and their verbs 66–69 (HTTPRoute header
matches, above); `tier()` places the predicate verbs with the plain ones, and the `silent_close`
and unix-backend verbs defensively. If the versions diverge again, verify that the wire format
still agrees before upgrading either side.

What 2.3.x changed that this codebase depends on (upstream commits, first released in 2.3.0
unless marked): `RemoveCluster` cascades (c2cad338) — re-expanded by the translator, see the diff
strategy above — and a removal whose cluster is gone answers `Ok` "nothing to remove" (same
commit); a frontend removal matches only the frontend of the cluster it names, for
HTTP/HTTPS too (TCP/UDP already did), which is why `sozu-agent` looks the stored owner up with
`ListFrontends` before evicting (see the shadow bullet); a UDP frontend address is exclusive across
clusters (044efef — the refusal, `UdpFrontendAddressTaken`, goes through that same owner eviction);
`AddBackend` for an absent cluster is refused with `ClusterNotFound` (8953343f, 2.3.2); the main
process's rollback of a fan-out no worker acknowledged grows by release — 2.2.1 reverted listener
and HTTP/HTTPS frontend adds; 2.3.0 also TCP/UDP frontend adds; 2.3.1 frontend removals
(6f1f1cc0: the stored frontend is restored, the source of the ghost above); 2.3.2 the
predicate/`silent_close` verbs and an inactive→active `ActivateListener`; path `Regex` rules are
anchored `\A(?:…)\z` (47eb07c) and compiled bare, then wrapped; `PathRule::eq` has its `Equals` arm
(795b95d); `Host` matches case-insensitively and certificate names are lowercased at load
(704b60d — inferred names are lowercased here too, for the main process's record); an exact host
whose rules miss falls through to a covering `*.` host before the hostname-less `POST` routes
(2ea35516 — Gateway listeners are not isolated, documented in features.md); a certificate name
with `/` is refused (e4aac48 — already refused via `validate_sni_pattern`); certificates are paired
with `keys_match`, and a same-fingerprint `ReplaceCertificate` whose names changed is applied in
place, fixing [#81](https://github.com/CleverCloud/sozu-gateway/issues/81) (0bc160b7, 2.3.2); a
repeated UDP `ActivateListener` keeps live flows (7109e20a); SIGTERM soft-stops Sōzu (57c92699 —
the `preStop` hook stays for its delay, and `sozu shutdown -t` is inert on a soft stop); an HTTP
connect timeout fails over to an untried backend and exhaustion answers 503 (e959838e — TCPRoute
still closes the session); a UDP cluster's `max_connections_per_ip` becomes a per-source flow cap
(99e6aee2 — the translator sends none on a UDP cluster). Unchanged on 2.3.2: the per-cluster
`QueryMetrics` worker wedge, TCP half-close (no reply after a client `SHUT_WR`, upstream
sozu#1840) and the rewrite/redirect/header-edit fields, all measured
([docs/probes/](docs/probes/)); one-label wildcards (#82); no rule priority (#80).

## Deployment model

Control plane (this repo) and data plane (Sōzu) are **separate processes/containers in one Pod**,
sharing the command socket via an `emptyDir` volume. Both run as the **same unprivileged uid
(1000)** so they can share that socket. The Helm chart ([charts/sozu-gateway](charts/sozu-gateway))
ships both containers, an `IngressClass`, RBAC, and Sōzu's `ConfigMap`. The Sōzu image is used
as-is (`clevercloud/sozu:2.3.2`) because the release binary is musl-linked.

Releases (`v*` tags) publish the controller image (`ghcr.io/clevercloud/sozu-gateway-controller`)
and the Helm chart (`oci://ghcr.io/clevercloud/sozu-gateway`) via
[.github/workflows/release.yml](.github/workflows/release.yml).

## Working notes

- `.scratch/` is local research/probe scaffolding (live-Sōzu protocol probes, recon notes behind
  PROTOCOL.md). It is gitignored, so it may not exist in a fresh clone; never rely on it.
- Errors: typed per-crate with `thiserror`; `anyhow` only in the controller binary.
- Code, comments, and docs are in English.
