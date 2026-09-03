// The user-facing binary is the same multicall entry point as `jcode`.
// Keep a distinct target file so Cargo does not report one source file as two
// build targets on every check.
include!("../main.rs");
