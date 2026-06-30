//! `hsd` — on-device daemon entry point.
//!
//! ## Android deployment
//!
//! In production this binary is cross-compiled to `aarch64-linux-android`
//! (via `cross`) and either:
//!
//! * invoked directly from a `setprop`/`init` boot service, or
//! * shimmed via `app_process` using a tiny `Main-Class` Java stub that
//!   calls `System.loadLibrary("hsd")` and then `Java_dev_handsets_HsdDaemon_main`.
//!
//! The cross-compile + `app_process` glue is **out of scope** for
//! Phase 2B. For now we build for the host so integration tests can
//! drive the dispatcher end-to-end.
//!
//! ## CLI
//!
//! ```text
//! hsd [addr:port] [max-conn N] [log-level LEVEL]
//! ```
//!
//! Positional args keep the surface small (one binary, three knobs).
//! `addr:port` defaults to `127.0.0.1:9008`; `max-conn` defaults to 8
//! to match handsets' Java backlog. `log-level` accepts
//! `error|warn|info|debug|trace` but for now only `info` (default) and
//! `error` do anything — we use `eprintln!` and the level just filters
//! the high-traffic "connection from ..." lines.
//!
//! Exit codes:
//! * `0` — clean shutdown (the `quit` verb was received)
//! * `1` — bind failed (port already in use, address parse error, etc.)
//! * `2` — accept loop returned an I/O error

#![deny(unsafe_code)]

use std::process::ExitCode;

use android_hid_daemon::server::{Server, ServerConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // ---- parse args ----------------------------------------------------
    let mut addr = "127.0.0.1:9008".to_owned();
    let mut max_connections: usize = 8;
    let mut log_level = LogLevel::Info;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return ExitCode::from(0);
            }
            "--version" | "-V" => {
                println!("hsd {}", android_hid_daemon::VERSION);
                return ExitCode::from(0);
            }
            "--max-conn" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    Some(n) if n > 0 => max_connections = n,
                    _ => {
                        eprintln!("hsd: --max-conn requires a positive integer");
                        return ExitCode::from(1);
                    }
                }
            }
            "--log-level" => {
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some("error") => log_level = LogLevel::Error,
                    Some("warn") => log_level = LogLevel::Warn,
                    Some("info") => log_level = LogLevel::Info,
                    Some("debug") => log_level = LogLevel::Debug,
                    Some("trace") => log_level = LogLevel::Trace,
                    Some(other) => {
                        eprintln!("hsd: unknown log level '{other}'");
                        return ExitCode::from(1);
                    }
                    None => {
                        eprintln!("hsd: --log-level requires a value");
                        return ExitCode::from(1);
                    }
                }
            }
            // Positional: first non-flag is `addr:port`.
            other if !other.starts_with('-') => {
                addr = other.to_owned();
            }
            other => {
                eprintln!("hsd: unknown argument '{other}' (try --help)");
                return ExitCode::from(1);
            }
        }
        i += 1;
    }

    // ---- bind + serve --------------------------------------------------
    let config = ServerConfig {
        bind_addr: match addr.parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("hsd: invalid bind address '{addr}': {e}");
                return ExitCode::from(1);
            }
        },
        max_connections,
        version: android_hid_daemon::VERSION,
    };

    log_at(
        log_level,
        &format!(
            "hsd: starting on {} (max_connections={}, version={})",
            config.bind_addr, config.max_connections, config.version
        ),
    );
    set_log_level(log_level);

    let server = match Server::bind(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hsd: bind failed: {e}");
            return ExitCode::from(1);
        }
    };

    match server.serve() {
        Ok(()) => {
            log_at(log_level, "hsd: clean shutdown");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("hsd: accept loop failed: {e}");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------------------
// Logging helper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

static LOG_LEVEL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(LogLevel::Info as u8);

fn set_log_level(level: LogLevel) {
    LOG_LEVEL.store(level as u8, std::sync::atomic::Ordering::Relaxed);
}

/// Emit `msg` to stderr if `level <= log_level`. Keeps the daemon
/// dependency-free (no `log` / `tracing` / `env_logger`).
fn log_at(level: LogLevel, msg: &str) {
    if (level as u8) <= LOG_LEVEL.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{msg}");
    }
}

fn print_help() {
    println!(
        "hsd — android-hid-connect on-device daemon\n\
         \n\
         USAGE:\n  \
             hsd [addr:port] [--max-conn N] [--log-level LEVEL]\n\
         \n\
         ARGS:\n  \
             addr:port            Bind address (default 127.0.0.1:9008)\n  \
             --max-conn N         Max concurrent client connections (default 8)\n  \
             --log-level LEVEL    error|warn|info|debug|trace (default info)\n  \
             -h, --help           Print this help and exit\n  \
             -V, --version        Print version and exit\n\
         \n\
         WIRE:\n  \
             Speaks the android-hid wire format (4-byte BE length-prefixed\n  \
             frames, zero-length = terminator, ERR:<UPPERCASE_TAG>:<detail>).\n\
         \n\
         HANDSHAKE:\n  \
             The daemon writes the literal 8-byte sequence PROTO/1\\n as\n  \
             the very first bytes of every accepted connection. Clients\n  \
             must read + verify those 8 bytes before sending their first\n  \
             frame.\n"
    );
}

// Tiny inline test that the help path at least parses args without
// panicking. Full CLI integration is exercised by the manual smoke
// tests in `server.rs`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_help_does_not_panic() {
        print_help();
    }

    #[test]
    fn log_level_default_is_info() {
        assert_eq!(
            LOG_LEVEL.load(std::sync::atomic::Ordering::Relaxed),
            LogLevel::Info as u8
        );
    }
}