# Automatic Gateway provisioning — 2026-09-10

**HTTPRouteMultipleGateways passes with automatically provisioned instances.**
This is a focused validation of PR #76, not a complete Gateway API conformance
run. The official HTTP report is **partial: 1 passed, 0 failed, 36 skipped**.

## Tested configuration

- Controller commit: `a8d54a3c5e03018316fe1fb0f262380d0da77d02`.
- Controller image digest: `sha256:4f7b6534fc7f2760aae91e9ca0a326ed0100bdfd13593ab910f82e3ea62a3982`.
- Sōzu: `clevercloud/sozu:2.2.1`.
- Kubernetes: 1.36.3, three existing nodes in `sozu-gateway-upgrade`.
- Gateway API standard CRDs and suite: v1.6.2; upstream commit
  `ca6c2a65454737236fb7a937bd9b17e42b07e9de`.
- Helm release `sozu-auto`, namespace `sozu-auto-test-0910`,
  GatewayClass `sozu-auto-0910`, controller name `sozu.io/auto-conformance-0910`.
- `gatewayProvisioning.enabled=true`, one replica per instance, ClusterIP
  Services. HTTP requests originate inside the cluster. No external load
  balancer or high-availability claim is made by these tests.
- Noncanonical CPU quantities (`0.1` / `0.25`) exercise normal API normalization.
  The no-repeated-PATCH guarantee is separately checked by mock API tests.

[Image build](artifacts/2026-09-10-provisioning/controller-build.json) and
[exact Helm values](artifacts/2026-09-10-provisioning/helm-values.json) identify
the deployed configuration. The test image is ephemeral; rebuild the recorded
commit when its registry retention expires.

## Official focused test

`HTTPRouteMultipleGateways` and all four cases passed: the shared route and
the independent default route through each Gateway. The two tested Gateway
addresses were `172.30.153.98` and `172.29.162.172`. All base fixtures, including
the HTTPS and namespace-selector Gateways, were left unchanged. No addresses
were substituted, no selected test was skipped, and the wrapper exited 0.
The Go suite took 36.74 seconds.

The compiled runner is from
[the conformance harness](https://github.com/CleverCloud/sozu-gateway/tree/b1c086061a120185756c1025af27edefa61191bc/tests/conformance),
with image digest
`sha256:03e2fa4d78b5f9bfa8a86fc36a9c581a781c25708e77ca5e99e57c93ca237378`.
Its catalog confirms the upstream revision above. The runner was invoked
inside the cluster with `--conformance-profiles=GATEWAY-HTTP` and
`--selected-tests=HTTPRouteMultipleGateways`, without `--probe-addresses`.
[Metadata](artifacts/2026-09-10-provisioning/official-metadata.json) records the
complete command and result; the [log](artifacts/2026-09-10-provisioning/official-suite.log)
and [unmodified report](gateway-provisioning_crd-v1.6.2_2026-09-10.yaml)
retain the official verdict. The [post-suite inventory](artifacts/2026-09-10-provisioning/official-cleanup-inventory.json)
records all four fixture Gateway UIDs and confirms that their generated resources
and fixture namespaces were gone at 16:13:07 UTC, before uninstalling the test
release.

## Lifecycle and ownership

The versioned [lifecycle test](../GATEWAY-PROVISIONING-E2E.md) creates two Gateways
after installation, with the same HTTP port and `/` path. On the recorded image:

| Check | Result |
|---|---|
| Independent default routes | 40 requests, 0 failures |
| Gateway deletion and same-name recreation | Old resources collected; new Gateway UID and instance; 40 requests, 0 failures |
| Recreated Gateway route changes backend | 40 requests, 0 failures |
| Traffic through the surviving Gateway during deletion/recreation | 36 fresh connections over 27.46 seconds, 0 failures or wrong backends |
| Worker workload permissions | Deployment creation denied in both installation and application namespaces |
| Installation template | UID, resourceVersion and content unchanged throughout the lifecycle test |
| Test cleanup | No cleanup errors; temporary namespace and instances removed |

These samples establish routing isolation over the measured interval, not a
lossless-startup or load benchmark. The 120 stage requests are separate from the
36 continuous samples. [Lifecycle summary](artifacts/2026-09-10-provisioning/lifecycle-summary.json)
and [HTTP identities](artifacts/2026-09-10-provisioning/lifecycle-http-identities.json)
record the measurements. The lifecycle, focused suite and recovery probe use
separate Gateways and namespaces on the same installation. The lifecycle and
official suite overlapped in time: the surviving Gateway sample also overlaps
creation of the official fixtures.

## Infrastructure recovery

A separate owned Gateway verified repair without changing or restarting the
provisioner. An external patch set its Deployment to zero replicas; the desired
one replica was restored 18.184 seconds after the patch response. Deleting its
Service with a UID precondition produced a replacement UID in 2.697 seconds.
The address changed from `172.28.187.64` to `172.25.13.231`, and the Gateway
published exactly the replacement address. These are observed convergence
bounds; Gateway status events may also wake the provisioner, so they do not
isolate the periodic resync path.

[Recovery summary](artifacts/2026-09-10-provisioning/recovery-summary.json)
records the mutations, identities and clean teardown. An
[initial probe](artifacts/2026-09-10-provisioning/recovery-initial-incomplete.json)
stopped before the drift mutation because kubectl did not accept a stdin patch
file. Its resources were cleaned up; the corrected probe above was the only
retry. The initial probe is not counted as a controller failure or a passed
recovery test.

## Local verification and limits

241 workspace tests, 13 Helm assertions, `just lint` and `just chart-lint` passed.
Mock API tests cover resource normalization without repeated writes, bounded
conflict retries, drift, same-name recreation, ownership refusal and status
publication races. They verify controller behavior against the mock, not the
exact HTTP code returned by a real apiserver for an invalid UID patch.

Provisioning remains opt-in because enabling it changes existing Gateway
addresses. Each generated Pod still contains a routing controller and Sōzu;
shared Kubernetes routing caches and remote configuration transport are not
implemented. HTTP listener ports still follow the chart exposure table. The
other conformance failures and native Sōzu limitations are unchanged by this
focused result. An infrastructure failure retries the global provisioning pass
after five seconds, so one persistent error can increase API request volume.

The test release, GatewayClass, build and runner namespaces, and runner RBAC
were removed after validation. [Cleanup verification](artifacts/2026-09-10-provisioning/cleanup-summary.json)
records the deletion requests and their completion. A subsequent
[API inventory](artifacts/2026-09-10-provisioning/cleanup-final-inventory.json)
confirms the original six namespaces, original GatewayClass, three Ready nodes
and seven Ready application/gateway Pods, with capture times and object UIDs.
