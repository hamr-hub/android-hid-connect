//! `android-hid-py` — Python SDK skeleton.
//!
//! **PyO3 integration deferred to Phase 7 — this stub keeps the workspace
//! member buildable.** Once Phase 7 lands, this crate will:
//!
//! 1. Add `pyo3` as an optional dependency gated behind a `pyo3` feature.
//! 2. Expose `android_hid.Agent` and friends via `#[pymodule]`.
//! 3. Ship a `pyproject.toml` `build-system` pointing at `maturin` so the
//!    extension module can be built and installed with one `pip install`.
//!
//! For Phase 1 the lib compiles to a `cdylib` with no Python bindings,
//! which keeps the workspace member buildable and lets `cargo test
//! --workspace` exercise it via the smoke test below.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

/// Phase-1 sentinel so the workspace has something to link. Phase 7
/// replaces this with a real `#[pymodule] fn android_hid(...)`.
#[allow(dead_code)]
const STUB_SENTINEL: &str = "android-hid-py stub — PyO3 lands in Phase 7";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_stub_compiles() {
        // Just touch the sentinel so dead-code lints don't complain, and
        // so the workspace has at least one assertion that the binding
        // crate is reachable from `cargo test --workspace`.
        assert!(STUB_SENTINEL.starts_with("android-hid-py"));
    }
}