//! Reloadable desktop2 application worker.
//!
//! The binary and this shared library intentionally compile the same current
//! Scene/Model/App implementation. The binary owns the native host. Self-dev
//! activation loads this library and swaps only the application callbacks.

// The worker intentionally includes the binary's complete application module
// graph, but host-only entry points are unreachable from the cdylib. They are
// live in the binary target, so library dead-code warnings are false positives.
#![allow(dead_code)]

include!("main.rs");
