#!/usr/bin/env bash
# Provision only a fresh, explicitly named Kind cluster for the official suite.
set -euo pipefail
kind_name=${1:?usage: kind.sh NAME}
context="kind-${kind_name}"
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
node_image='kindest/node:v1.36.1@sha256:3489c7674813ba5d8b1a9977baea8a6e553784dab7b84759d1014dbd78f7ebd5'
if kind get clusters | grep -Fxq "$kind_name"; then
  echo "Refusing an existing Kind cluster: $kind_name" >&2
  exit 2
fi
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo 'created=true' >> "$GITHUB_OUTPUT"
fi
kind create cluster --name "$kind_name" --image "$node_image" --wait 180s
kubectl --context "$context" apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.2/standard-install.yaml
kubectl --context "$context" apply -f https://raw.githubusercontent.com/metallb/metallb/v0.16.0/config/manifests/metallb-native.yaml
kubectl --context "$context" -n metallb-system wait --for=condition=Available deployment/controller --timeout=180s
kubectl --context "$context" -n metallb-system rollout status daemonset/speaker --timeout=180s
kubectl --context "$context" label nodes --all node.kubernetes.io/exclude-from-external-load-balancers- --overwrite
# Reserve addresses at the high end of Kind's Docker subnet, away from nodes.
# MetalLB publishes these actual VIPs; neither the runner nor fixture addresses
# are rewritten to ClusterIPs or port-forward endpoints.
network=$(docker network inspect kind)
pool=$(python3 -c 'import ipaddress,json,sys
config=json.load(sys.stdin)[0]["IPAM"]["Config"]
net=next(ipaddress.ip_network(c["Subnet"]) for c in config if ":" not in c["Subnet"])
assert net.num_addresses >= 256
print(str(net[-50])+"-"+str(net[-10]))' <<< "$network")
kubectl --context "$context" apply -f - <<EOF
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: conformance
  namespace: metallb-system
spec:
  addresses: ["$pool"]
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: conformance
  namespace: metallb-system
spec:
  ipAddressPools: [conformance]
EOF
docker build --tag sozu-gateway-controller:conformance "$root"
docker build --tag sozu-gateway-conformance:local "$root/tests/conformance"
kind load docker-image --name "$kind_name" sozu-gateway-controller:conformance sozu-gateway-conformance:local
helm upgrade --install sozu-gateway "$root/charts/sozu-gateway" --kube-context "$context" \
  --namespace sozu-system --create-namespace --values "$root/tests/conformance/values.yaml" --wait --timeout 5m
kubectl --context "$context" apply -f - <<EOF
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: sozu
spec:
  controllerName: sozu.io/gateway-controller
EOF
kubectl --context "$context" wait --for=jsonpath='{.status.loadBalancer.ingress[0].ip}' service/sozu-gateway -n sozu-system --timeout=180s
kubectl --context "$context" wait --for=condition=Accepted gatewayclass/sozu --timeout=180s
