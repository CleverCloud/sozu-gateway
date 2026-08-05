//! Gateway API CRD types (`gateway.networking.k8s.io`).
//!
//! Generated from the upstream **v1.6.1** standard-channel CRDs with `kopium`
//! (regeneration steps in [README.md](README.md)). Types only: `kube` is pulled
//! for the `CustomResource` derive (which provides the `kube::Resource` impl the
//! watchers need), not for any client or runtime I/O.
//!
//! Every kind here is generated from its **served** API version, which is not
//! always the newest one the CRD declares: `ReferenceGrant` is pinned to
//! `v1beta1`, the only version served across the whole range of Gateway API
//! releases this controller supports (`v1` appeared in v1.5). Getting that wrong
//! is silent — the type compiles, and the controller's CRD probe takes a real
//! 404 on older clusters and quietly drops to Ingress-only.
//!
//! The modules are generated code; lints are relaxed on them so the rest of the
//! workspace can keep `clippy -D warnings`.
#![forbid(unsafe_code)]

#[allow(clippy::all, non_snake_case)]
pub mod gateway;
#[allow(clippy::all, non_snake_case)]
pub mod gatewayclass;
#[allow(clippy::all, non_snake_case)]
pub mod httproute;
#[allow(clippy::all, non_snake_case)]
pub mod referencegrant;
#[allow(clippy::all, non_snake_case)]
pub mod tcproute;
#[allow(clippy::all, non_snake_case)]
pub mod udproute;

pub use gateway::{Gateway, GatewaySpec, GatewayStatus};
pub use gatewayclass::{GatewayClass, GatewayClassSpec, GatewayClassStatus};
pub use httproute::{HttpRoute, HttpRouteSpec, HttpRouteStatus};
pub use referencegrant::{ReferenceGrant, ReferenceGrantSpec};
pub use tcproute::{TcpRoute, TcpRouteSpec, TcpRouteStatus};
pub use udproute::{UdpRoute, UdpRouteSpec, UdpRouteStatus};
