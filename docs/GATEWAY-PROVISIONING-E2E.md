# Automatic Gateway provisioning lifecycle test

`scripts/e2e-gateway-provisioning.py` tests an existing installation with
`gatewayProvisioning.enabled=true` and
`gatewayProvisioning.service.type=ClusterIP`. The installation must use a
controller name distinct from other test installations, with a GatewayClass
whose `spec.controllerName` matches it. The script does not install or upgrade
Helm releases, create GatewayClasses, or change the installation configuration.

Requirements: Python 3 and `kubectl` locally, two spare backend Pods, and enough
capacity for two Gateway instances at the installation's configured replica
count. The caller needs permission to create a temporary namespace and its
fixtures, read generated resources, and impersonate the workers' ServiceAccount
for the negative RBAC checks.

```sh
python3 scripts/e2e-gateway-provisioning.py \
  --context sozu-gateway-upgrade@kubernetes_01M257SQRH3TT7QCXPQB7RM1FH \
  --namespace sozu-provisioning-test \
  --gateway-class sozu-provisioning-test \
  --output .scratch/provisioning-lifecycle-run-1
```

The output directory must not already exist. `--runner namespace/pod` can reuse
an existing Pod with `python3`; otherwise the script creates a temporary
`python:3.13-alpine` probe Pod. HTTP requests originate inside the cluster and
use fresh connections. Backend Pods use
`registry.k8s.io/gateway-api/echo-basic:v1.5.1`, reporting their Pod and namespace
identities. These test image tags are recorded in the script; runtime Pod image
IDs are retained in the evidence when available.

The test creates two Gateways **after** the installation, with identical HTTP
listener ports, no hostnames, and independent `/` routes. It checks:

1. Automatic Deployment and ClusterIP Service creation, exact installation and
   Gateway UID selectors, current Gateway/HTTPRoute status, and distinct backend
   responses through both addresses.
2. Worker ServiceAccounts cannot create Deployments in either the installation
   namespace or the temporary application namespace.
3. Deleting one Gateway removes its generated resources. Recreating that name
   creates a new UID and new resources. The other Gateway keeps its original
   Deployment and Service, with continuous HTTP sampling during this lifecycle.
4. Updating the recreated Gateway's route changes its backend. The installation
   template remains unchanged throughout the test.

Each measured traffic stage requires twenty successful fresh connections per
Gateway. Startup convergence attempts are recorded separately, including failed
requests; a successful final stage does not imply lossless startup. The continuous
survivor sample allows no failed or misrouted responses, but only establishes
behavior for the measured interval and sample rate. This lifecycle test does not
replace the upstream Gateway API conformance suite.

Evidence includes `summary.json`, timestamped `events.jsonl`, HTTP responses,
Gateway/route status, generated resource identities, Pod runtime image IDs, and
RBAC answers. It does not read Kubernetes Secrets or store kubeconfig credentials.
The exit status is nonzero on assertion failure or incomplete cleanup.

Cleanup runs after success and failure. Gateway and namespace deletions carry
the exact captured UID as an API precondition. The script waits for the
provisioner to remove only resources bearing both the installation UID and the
test Gateway UID. It then removes its own temporary namespace. The installation,
GatewayClass, and an explicitly supplied runner are retained. An interrupted
process may need manual cleanup using the identities in the evidence; finalizers
are never forced.
