//! sozu-agent: thin, typed wrapper around `sozu-command-lib`'s command socket.
//!
//! Owns all socket I/O (connect, send, ack, `LoadState`, reconnect). The pure
//! crates (`ir`, `translator`) never touch this. Fleshed out in Étape 3 once
//! the wire protocol is confirmed and documented in `PROTOCOL.md`.
#![forbid(unsafe_code)]

// Implemented in Étape 3.
