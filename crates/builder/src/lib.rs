//! Builder: Kubernetes objects -> IR.
//!
//! Resolves references (Service -> EndpointSlice, TLS Secret), validates, and
//! produces the IR plus per-object status results. No live API I/O: it operates
//! on already-fetched typed objects so it can be unit-tested in isolation.
#![forbid(unsafe_code)]

// Implemented in Étape 4 (after PROTOCOL.md + IR + Translator).
