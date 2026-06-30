//! `ah` — android-hid CLI entry point.
//!
//! Phase 1 placeholder: prints the version and exits. Phase 7 will
//! wire up argument parsing, agent dispatch, and the full verb set
//! (input, clipboard, files, pm/am/wm, screenshots, etc.).

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

mod args;

use std::process::ExitCode;

fn main() -> ExitCode {
    let _args = args::Args;
    println!("ah 0.1.0");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_args_default_compiles() {
        let a = args::Args;
        let _ = a;
    }

    #[test]
    fn main_version_string_is_stable() {
        // Keep the printed version grep-friendly for tests/CI.
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
    }
}