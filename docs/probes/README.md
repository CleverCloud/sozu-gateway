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

| Probe | Question |
| ----- | -------- |
| `rewrite_redirect_probe` | What `rewrite_host` / `rewrite_path` / `rewrite_port` actually do, on the forwarding path and under each redirect policy — see [PROTOCOL.md §13](../../PROTOCOL.md) |
