//! Intermediate Representation (IR) for the Sōzu gateway controller.
//!
//! Neutral, I/O-free Rust structures mapped 1:1 onto Sōzu's vocabulary
//! (`Listener`, `Cluster`, `Frontend`, `Backend`, `Certificate`). The Builder
//! produces this from Kubernetes objects; the Translator consumes it to emit
//! Sōzu protobuf commands. This crate must not depend on `kube` or the socket.
#![forbid(unsafe_code)]

// Types are added in Étape 2 (after PROTOCOL.md is validated).
