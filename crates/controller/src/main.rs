//! Sōzu gateway controller binary.
//!
//! kube-rs Controller runtime wiring: watch Ingress/IngressClass/Service/
//! EndpointSlice/Secret, build IR, translate to Sōzu commands, push over the
//! command socket. Implemented in Étape 4.

fn main() -> anyhow::Result<()> {
    println!("sozu-gw-controller: skeleton (Étape 1)");
    Ok(())
}
