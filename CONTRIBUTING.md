# Contributing to sozu-gateway

Thank you for your interest in sozu-gateway, you're very welcome here! This is a sibling
project to [Sōzu](https://github.com/sozu-proxy/sozu) — a Kubernetes Ingress controller and
API gateway that *drives* a co-located Sōzu, rather than the proxy itself — so if you've
contributed to Sōzu before, much of this will feel familiar.

## Communication

Most discussion happens in the GitHub [issues](https://github.com/CleverCloud/sozu-gateway/issues).
Open one to report a bug, propose a feature, or ask a question before sinking time into a large
change — it's the cheapest way to get alignment. Pull requests are welcome too; for anything beyond
a small fix, an issue first saves everyone the round-trip.

## Navigating the source

sozu-gateway is designed as a **minimal control plane that you drive a co-located Sōzu through**.
The controller watches Kubernetes objects, compiles them into a neutral intermediate
representation (IR), diffs that IR against the last-applied state, and pushes the minimal set of
mutations to Sōzu over its protobuf **command socket** — hot, with no proxy restarts. The proxy is
treated as an opaque, well-behaved data plane; all the cleverness lives in *how* we talk to it.

The workspace is split into seven crates, layered so the pure ones can be unit-tested without a
cluster or a socket. This **purity boundary is load-bearing** — it is what keeps the translator and
builder fast, deterministic, and golden-snapshot-testable.

```
K8s objects ─▶ reflector caches ─▶ builder ─▶ IR ─▶ translator ─▶ protobuf cmds ─▶ Sōzu socket
```

- [`crates/ir`](crates/ir) (`sozu-gw-ir`) — the neutral IR structs (`Cluster`, `Backend`,
  `Frontend`, `Certificate`, `Ir`), mapped 1:1 onto Sōzu's routing vocabulary. **No I/O.** It
  depends only on `serde`.
- [`crates/gateway-api`](crates/gateway-api) (`sozu-gw-gateway-api`) — `kopium`-generated Gateway
  API CRD types (v1.2.1 standard channel). **Types only — do not hand-edit; regenerate per the
  crate's README.** `kube` is pulled in for the `CustomResource` derive, not for client/runtime I/O.
- [`crates/builder`](crates/builder) (`sozu-gw-builder`) — maps typed Ingress **and** Gateway API
  objects into the IR, emitting non-fatal `Problem`s for anything Sōzu can't represent. Resolves
  Service → EndpointSlice pod IPs and validates TLS Secrets. **No kube-client I/O, no socket.**
- [`crates/translator`](crates/translator) (`sozu-gw-translator`) — pure IR → Sōzu protobuf
  commands, diffed against the last-applied state. Two diff strategies live here: the routing graph
  reuses `sozu-command-lib`'s `ConfigState::diff` so semantics match the data plane exactly, while
  certificates are diffed by hand keyed on `(listener, fingerprint)` for zero-gap rotation.
  **No socket I/O.**
- [`crates/prometheus`](crates/prometheus) (`sozu-gw-prometheus`) — pure Sōzu `AggregatedMetrics` →
  Prometheus text exposition. **No socket/kube I/O.**
- [`crates/sozu-agent`](crates/sozu-agent) (`sozu-gw-agent`) — a thin typed wrapper around
  `sozu-command-lib`'s command socket. **Owns all socket I/O:** connect, idempotent batch send
  (loop on `Processing` → `Ok`/`Failure`), bounded reads, reconnect-and-retry.
- [`crates/controller`](crates/controller) (`sozu-gw-controller`) — the binary. `kube-rs` watchers
  feed one singleton, global, debounced reconcile that rebuilds the entire desired IR, diffs it
  against an in-memory shadow, and applies only the delta. **Holds all kube + socket I/O**, plus
  status writes, metrics, health, and shadow persistence.

**Keep `ir`, `gateway-api`, `builder`, `translator`, and `prometheus` free of socket and kube-client
I/O.** All such I/O belongs in `sozu-agent` and `controller`. Violating this boundary breaks unit
testability and is the single easiest way to get a change bounced in review.

## Building and running

You need:

- **`protoc` (`protobuf-compiler`)** — `sozu-command-lib`'s `build.rs` runs `prost-build`, so the
  build will not even start without it. On a bare host: `apt-get install protobuf-compiler`.
- **[`just`](https://github.com/casey/just)** — the [`justfile`](justfile) is the authoritative
  source for task and command names. Run `just` with no args to list every recipe.
- **Rust 1.88 (stable)**, edition 2021. No nightly is required — unlike Sōzu, formatting and lints
  run on the stable toolchain.
- The easiest path is the **devcontainer** ([`.devcontainer`](.devcontainer)): it installs `protoc`,
  `just`, the Rust toolchain, and a Kubernetes stack (kubectl/helm/minikube) for you.

The core loop:

```bash
just build          # cargo build --workspace
just test           # cargo test --workspace (unit + golden/snapshot tests)
just lint           # fmt-check + clippy -D warnings (the CI gate)
just fmt            # cargo fmt (write)
just image          # docker build the controller image
just chart-lint     # helm lint + template renders of the chart
```

Override variables before the recipe, e.g. `just IMAGE=my/repo TAG=v0.2.0 image`.

## Debugging the data plane

The Sōzu command socket speaks a **bare length-prefixed `Request`** (not a `WorkerRequest`
envelope); replies come back as `Processing` → `Ok`/`Failure`, so every send loops until a terminal
status. This protocol is verified against a live Sōzu and documented in
[`PROTOCOL.md`](PROTOCOL.md) — **the source of truth for the translator.**

- **Never reimplement the wire format or invent protobuf fields.** Reuse `sozu-command-lib`'s types
  and conversions (e.g. `addr.into()`, never hand-pack an address). If `PROTOCOL.md` and the code
  disagree, that's a bug to file, not a thing to route around.
- Raw probe notes from reverse-engineering the socket live in [`.scratch/recon/`](.scratch/recon).
  All of `.scratch/` is research scaffolding, **not part of the shipped product** — don't import
  from it.
- For runtime logs, the controller uses `tracing`; turn up verbosity with `RUST_LOG`, e.g.
  `RUST_LOG=sozu_gw_controller=debug,sozu_gw_agent=debug`. Sōzu's own logging is configured in its
  [`config.toml`](deploy/sozu/config.toml).

Remember the deployment shape when reproducing socket issues: control plane (this repo) and data
plane (`clevercloud/sozu:2.1.0`) run as separate containers in one Pod, sharing the command socket
via an `emptyDir` volume, both as uid `1000`.

## Testing

Testing is non-negotiable here. **Before opening a PR, run the same chain CI runs** and make sure
it's green end to end:

```bash
just fmt-check      # cargo fmt --check
just clippy         # cargo clippy --workspace --all-targets -- -D warnings
just test           # cargo test --workspace
just chart-lint     # helm lint + template (the Helm gate)
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `just fmt-check`, `just clippy`
(warnings denied), and `just test`, plus the Helm chart-lint and Docker image-build jobs, on every
push to `master`/`main` and on all pull requests. If `just lint` and `just test` pass locally, the
`build-test-lint` job should pass too.

Most of the test weight is **golden/snapshot tests** using [`insta`](https://insta.rs). Snapshots
live in `crates/builder/tests/snapshots/`, `crates/translator/tests/snapshots/`, and
`crates/prometheus/tests/snapshots/`. The builder snapshots pin IR output for Ingress and Gateway
API inputs; the translator snapshots pin the exact `Request` stream and reconciliation behavior
(scale up/down, cert rotation, route retargeting, L4 listener add/remove, filter mapping); the
prometheus snapshots pin the exposition text.

When an intentional change moves a snapshot, review and accept it deliberately:

```bash
cargo insta review              # interactive: inspect each diff, accept or reject
# or, to accept everything in one shot:
INSTA_UPDATE=always cargo test
```

To run a single test, target the crate:

```bash
cargo test -p sozu-gw-translator <name>
```

(crates: `sozu-gw-ir`, `sozu-gw-gateway-api`, `sozu-gw-builder`, `sozu-gw-translator`,
`sozu-gw-prometheus`, `sozu-gw-agent`, `sozu-gw-controller`.)

For end-to-end coverage there are three shipped suites, each running in-cluster on your current
kube-context (they default to ephemeral [ttl.sh](https://ttl.sh) images, so no registry creds are
needed — ttl.sh is anonymous and world-writable, which is why the suites deploy by the *digest*
resolved from their own push, and why you should export `IMAGE=<your registry>` for anything
beyond a throwaway cluster):

```bash
just e2e            # Ingress + TLS
just e2e-gateway    # Gateway API + HTTPRoute filters
just e2e-l4         # raw TCP/UDP listeners
just e2e-all        # all three, sharing one built image
```

Results are recorded in [`docs/E2E-RESULTS.md`](docs/E2E-RESULTS.md).

A handful of rules keep the suite honest:

- **Tests ship in the same changeset as the change they cover.** A behavior change without a test
  (or snapshot) update is incomplete.
- **A `.snap` diff is a behavior change to scrutinize, not a thing to blindly re-bless.** If you
  can't explain *why* the snapshot moved, don't accept it.
- **Keep the pure crates pure.** `ir`/`gateway-api`/`builder`/`translator`/`prometheus` must stay
  free of socket and kube-client I/O so they remain trivially unit-testable.
- **Never `panic!`.** Errors are typed per-crate with `thiserror` (`anyhow` only in the controller
  binary); the controller's discipline is **fail-fast** — on a dead watch stream or unsynced caches
  it *exits* so Kubernetes restarts it, rather than silently going blind. Network-controlled input
  must never bring the process down.
- **Don't hardcode a registry in e2e.** The image registry is runtime-resolved (`ttl.sh` by default,
  overridable via `$IMAGE`); keep the suites portable across clusters.
- **Any `#[ignore]`d test carries a reason string** explaining why it's skipped (e.g. needs a live
  Sōzu), so a skipped test never goes silently unexplained.
- **Code, comments, and docs are in English.**

## Submitting changes

- **Use [Conventional Commits](https://www.conventionalcommits.org/):** `feat:`, `fix:`, `docs:`,
  `test:`, `chore:`, `build:`. The history is consistent — please keep it that way.
  - **Add a scope to the header whenever the change is localized to one area** —
    `feat(controller):`, `fix(translator):`, `feat(metrics):`, `docs(readme):`. Drop it only when a
    change genuinely spans the whole workspace.
  - **The body explains _why_, not _what_.** The diff already shows what changed; spend the body on
    the motivation, constraint, or trade-off the code can't convey. Omit the body only when the
    subject is self-explanatory.
- **Keep CI green.** A PR that doesn't pass `just lint && just test` (and the Helm/Docker jobs) isn't
  ready for review.
- **Branch from and target `master`** (the default branch).
- **Keep PRs small and focused.** One logical change per PR reviews far better than a grab-bag.
- **Update the docs when behavior changes.** [`README.md`](README.md),
  [`CLAUDE.md`](CLAUDE.md) (symlinked to `AGENTS.md`), [`PROTOCOL.md`](PROTOCOL.md), and
  [`docs/`](docs) should never lag the code.

## Licensing

sozu-gateway is licensed under **Apache-2.0** (see [`LICENSE`](LICENSE)). By contributing, you agree
that your contributions are submitted under, and will be distributed under, that same license.

There is **no CLA, no copyright assignment, and no DCO sign-off requirement** — just open a PR. (This
is a deliberate divergence from upstream Sōzu, which is AGPL-3 and asks contributors to sign a CLA.)
