//! Translator: pure IR -> Sōzu protobuf commands.
//!
//! Diffs the desired IR against the last-applied state and emits only the
//! minimal set of mutations. Entirely side-effect free and golden-file tested.
#![forbid(unsafe_code)]

// Implemented in Étape 2 (after PROTOCOL.md is validated).
