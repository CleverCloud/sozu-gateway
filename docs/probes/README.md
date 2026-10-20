# Probe runs

Raw output of the measurement probes under `crates/sozu-agent/examples/`, kept
verbatim so a claim in [PROTOCOL.md](../../PROTOCOL.md) or
[E2E-RESULTS.md](../E2E-RESULTS.md) can be traced to the run it came from — and
so a later Sōzu version can be diffed against the same questions rather than
re-argued from doc comments.

Files are named `<probe>_<sozu-version>_<date>.txt` and are **immutable**: a new
run gets a new file. A result that changes between versions is the interesting
part; overwriting it hides exactly that. This is how the URLRewrite `408`
recorded against Sōzu 2.1.0 went unchallenged long enough to reach the feature
matrix as a hard limit.

## Running one

The probes talk to the command socket, so they need to run somewhere that mounts
it. Build an image carrying both the controller and the probe, install the chart
with it, and exec:

```sh
cargo build --release -p sozu-gw-controller
cargo build --release -p sozu-gw-agent --example rewrite_redirect_probe

# any image build that lands both binaries in /usr/local/bin works;
# scripts/build-image-nodocker.sh does the controller half without docker
helm upgrade --install sozu-gateway charts/sozu-gateway -n sozu-system \
  --set image.controller.repository=... --set controller.resyncSecs=0 --wait

kubectl exec -n sozu-system deploy/sozu-gateway -c controller -- \
  /usr/local/bin/rewrite_redirect_probe | tee docs/probes/<name>.txt
```

`controller.resyncSecs=0` with no routes applied keeps the controller idle: it
diffs its own shadow, never Sōzu's live state, so the frontends a probe programs
out of band are left alone.

The `grpc-*` records are the exception: they come from an out-of-tree harness
— shell scripts running docker `clevercloud/sozu` in front of small Go servers,
with grpc-go 1.75.0 clients, curl and Python — that is not in this repository.
Each file's header says how it was run and what each host or cluster name in it
stands for; the raw output follows it verbatim.

| Probe | Question |
| ----- | -------- |
| `grpc-backend-changes` (out of tree) | Whether open gRPC streams survive a backend's GOAWAY (GracefulStop) or its removal from the cluster, and whether RPCs are balanced per RPC or per client connection — see [Upgrading](../UPGRADING.md#a-service-port-declaring-kubernetesioh2c-gets-http2) |
| `grpc-cleartext` (out of tree) | What Sōzu's HTTP listener does with an HTTP/2 prior-knowledge preface and with an `Upgrade: h2c` offer (to an HTTP/1.1 backend that ignores it); also trailer passthrough, with a probe bug in its request-trailer cases |
| `grpc-errors` (out of tree) | Which gRPC status a client derives from Sōzu's own `502`/`503`/`504`/`404` answers, and what a backend killed mid-stream does to the stream |
| `grpc-http2-flip` (out of tree) | Whether flipping a cluster's `http2` (`sozu cluster h2`, an `AddCluster`) reaches a keep-alive connection already open, in both directions |
| `grpc-idle-timeouts` (out of tree) | Which timeout bounds a silent gRPC stream once `back_timeout` is raised, and whether raising `back_timeout` and `front_timeout` together lifts it |
| `grpc-quick-interop` (out of tree) | Whether an h2c-only backend behind an `http2` cluster serves HTTP/1.1 keep-alive and HTTP/2 clients, 64 MiB both ways and `TE: trailers`; what an HTTP/1.1-only backend behind `http2`, and an h2c-only one without it, answer; grpc-go's interop cases over HTTPS and over the cleartext listener |
| `grpc-streams` (out of tree) | How long a gRPC stream may stay silent, with and without client PINGs; whether a stream with steady traffic outlives that; large messages; 300 concurrent calls on one connection |
| `grpc-verify` (out of tree) | A later re-check, on a two-worker config: the cleartext refusal, the interop cases, error mapping, an `AddCluster` without `http2` after one with it, and a WebSocket handshake to an h2c port |
| `h2c_backend_probe` | Whether a backend that speaks only h2c reaches HTTP/2 through Sōzu when its cluster carries `http2` (the `appProtocol: kubernetes.io/h2c` mapping) — from an HTTP/1.1 and an HTTP/2 client on the HTTPS listener and an HTTP/1.1 client on the HTTP listener — and what fails without it; plus a cleartext prior-knowledge h2c client, an HTTP/1.1-only backend behind `http2`, and a WebSocket upgrade to a port serving both. Programs Sōzu through IR → translator → agent and needs `curl` with HTTP/2. Runs against a local `sozu start` with `SOZU_SOCK`/`PROBE_HTTP`/`PROBE_HTTPS` pointed at it — see [Upgrading](../UPGRADING.md#a-service-port-declaring-kubernetesioh2c-gets-http2) |
| `metrics_probe` | What `QueryMetrics` returns with and without `no_clusters`, and whether the per-cluster query wedges the workers (pair it with a lowered `max_command_buffer_size`). It also runs against a local `sozu start`, with `SOZU_SOCK`/`PROBE_HTTP_*` pointed at it — see [Upgrading](../UPGRADING.md#metrics-exports-proxy-wide-sōzu-series-only) |
| `owner_eviction_probe` | Whether the agent converges when Sōzu stores a frontend under a cluster the shadow never knew — an add for another cluster on the same HTTP route key or L4 address, and a removal naming the wrong cluster — read back from the main process's own frontend list. Runs against a local `sozu start` with `SOZU_SOCK`/`PROBE_HTTP` pointed at it |
| `partial_apply_probe` | Whether the controller's baseline converges after a batch Sōzu applied only in part, for four sequences: a batch removing a cluster fails on a later request, then the cluster is wanted back with a new backend; a batch re-pointing a route to another live cluster fails the same way, then the route is deleted; a batch switching a UDP route's cluster to another algorithm while dropping the route fails right after the cluster's upsert, then the route is wanted back (`udp`) or the first state is (`udp-revert`). Each runs the controller's loop four times, the shadow after the failure kept unchanged, replayed with the acknowledged removals alone, replayed with every acknowledged request, and replayed only when that describes the routing state exactly, and reads each step back from the main process's frontend list (and the UDP cluster from the main process and each worker). Runs against a local `sozu start` with `SOZU_SOCK`/`PROBE_HTTP`/`PROBE_HTTPS` pointed at it, `PROBE_UDP_PORT` the first of eight free UDP ports |
| `replace_certificate_fallback_probe` | Whether a certificate rotation lands on a listener Sōzu holds no certificate for — the state a shadow lagging behind a landed `RemoveCertificate` leaves — sent raw and through the agent, read back from the main process's certificate list (the served leaf is checked with `openssl s_client`). Runs against a local `sozu start` with `SOZU_SOCK`/`PROBE_HTTPS`/`PROBE_CERT_*`/`PROBE_KEY_*` pointed at it |
| `sni_ownership_probe` | Which certificate a handshake gets when two certificates at one HTTPS listener name the same SNI (the later-expiring one, on 2.2.1 and 2.3.2), and whether stripping the name from the newer claimant's certificate with the translator's same-fingerprint `ReplaceCertificate` restores the other on a running Sōzu (2.3.2 only; 2.2.1 acknowledges it and keeps serving). It also races handshakes against the name changing hands in one batch: in the translator's order, then with the batch that gives the name (the order `CertRank` fixes) and the one that takes it back (ordered by the tiers) each reversed on its own, then both (`PROBE_ROUNDS` round trips each; the `sni-ownership_*` runs predate that split and reverse both). Runs against a local `sozu start` (or a container) with `SOZU_SOCK`/`PROBE_HTTPS`/`PROBE_CONNECT` pointed at it |
| `rewrite_redirect_probe` | What `rewrite_host` / `rewrite_path` / `rewrite_port` actually do, on the forwarding path and under each redirect policy, plus whether a `Header` `set` replaces or appends — see [PROTOCOL.md §13](../../PROTOCOL.md) |

The probes also run against a local `docker run --rm --network host --user 1000:1000
clevercloud/sozu:<version>` with the socket directory bind-mounted from the host (keep the
socket path under 108 bytes, through a symlink if need be); each record's header says how it was
run.

## Records

| Question | Sōzu | Record |
| -------- | ---- | ------ |
| `metrics_probe` | 2.2.1 | [metrics-no-clusters_sozu-2.2.1_2026-09-23.txt](metrics-no-clusters_sozu-2.2.1_2026-09-23.txt) |
| `metrics_probe` (runs 1–2 of the 2.2.1 record, docker) | 2.3.2 | [metrics-no-clusters_sozu-2.3.2_2026-10-19.txt](metrics-no-clusters_sozu-2.3.2_2026-10-19.txt) |
| `owner_eviction_probe` | 2.2.1, 2.3.2 | [owner-eviction_sozu-2.2.1_2026-10-19.txt](owner-eviction_sozu-2.2.1_2026-10-19.txt), [owner-eviction_sozu-2.3.2_2026-10-19.txt](owner-eviction_sozu-2.3.2_2026-10-19.txt) |
| `rewrite_redirect_probe` | 2.2.0 | [rewrite-redirect_sozu-2.2.0_2026-08-04.txt](rewrite-redirect_sozu-2.2.0_2026-08-04.txt), [rewrite-redirect-headers_sozu-2.2.0_2026-08-05.txt](rewrite-redirect-headers_sozu-2.2.0_2026-08-05.txt), [redirect-host-targets_sozu-2.2.0_2026-08-11.txt](redirect-host-targets_sozu-2.2.0_2026-08-11.txt) |
| `rewrite_redirect_probe` (extended: every `Host` line, capturing rules, `$$`, `Host` edits, CR/LF) | 2.2.1, 2.3.2 | [rewrite-redirect_sozu-2.2.1_2026-10-19.txt](rewrite-redirect_sozu-2.2.1_2026-10-19.txt), [rewrite-redirect_sozu-2.3.2_2026-10-19.txt](rewrite-redirect_sozu-2.3.2_2026-10-19.txt) |
| TCP half-close (no example; the method is in each record) | 2.2.0, 2.2.1 | [tcp-half-close_2026-09-10.md](tcp-half-close_2026-09-10.md) |
| TCP half-close (local docker, Python client) | 2.3.2 | [tcp-half-close_sozu-2.3.2_2026-10-19.txt](tcp-half-close_sozu-2.3.2_2026-10-19.txt) |
| Container restart detection | 2.2.1 | [restart-generation_2026-09-10.md](restart-generation_2026-09-10.md) |
