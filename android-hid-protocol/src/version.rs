//! Wire-protocol version constants.

/// Current `android-hid-protocol` wire version.
///
/// Bump this whenever a wire change (new verb, new error code, new
/// length-prefix format, etc.) is incompatible with older clients.
pub const PROTOCOL_VERSION: u32 = 1;

/// User-Agent string this crate identifies itself as on the wire.
///
/// Used by the CLI banner and by `android-hid-agent` for handshake
/// versioning. Format follows `name/major.minor` to match the
/// `Rust-x.y` convention so it stays greppable in logs.
pub const USER_AGENT: &str = "android-hid-agent/0.1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_v1() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn user_agent_is_set() {
        assert!(!USER_AGENT.is_empty());
        assert!(USER_AGENT.contains('/'));
    }
}