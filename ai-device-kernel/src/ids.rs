//! Server-assigned identifier types for the AI Device Kernel.
//!
//! Every entity that needs idempotency or cross-referencing gets a
//! typed ID here. IDs are wire-stable: a replayed frame should always
//! produce the same ID for the same operation type, so retries are safe
//! across agent restarts.
//!
//! ## Design rationale
//!
//! - **ActionId / PlanId / StepId / PredicateId** = u64 — server-assigned,
//!   monotonic, `#[serde(transparent)]`-style postcard encoding (8 bytes
//!   per ID, no length prefix overhead).
//! - **ScreenId** = `[u8; 16]` blake3 digest of `(a11y-hash ‖ frame-pHash
//!   ‖ pkg:activity)` — cross-device/cross-time stable per v3 §3.2.0,
//!   used as Memory layer key.
//! - All IDs derive `Hash + Eq + Copy` where possible so the host can
//!   use them as `HashMap` keys without boxing.
//!
//! See v3 §3.2.0 (ScreenId), §3.2.1 (ActionId), §3.2.2 (PlanId/StepId),
//! §3.2.4 (PredicateId) for the design intent.

use serde::{Deserialize, Serialize};

/// Server-assigned action ID. u64 → 8 bytes on the wire, monotonic
/// across the daemon lifetime. Used for idempotent retry: a replayed
/// `Action` carrying the same `ActionId` returns the original result.
///
/// Manual `Serialize`/`Deserialize` so postcard bypasses the
/// tuple-struct tag byte (8 bytes raw, not 8 + 1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct ActionId(pub u64);

impl Serialize for ActionId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_u64(ActionIdVisitor)
    }
}

struct ActionIdVisitor;

impl<'de> serde::de::Visitor<'de> for ActionIdVisitor {
    type Value = ActionId;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an ActionId (u64)")
    }
    fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Self::Value, E> {
        Ok(ActionId(n))
    }
}

impl ActionId {
    /// Reserved for the "no action performed" sentinel
    /// (e.g. when a Step is skipped due to `wait_before` failure).
    pub const ZERO: Self = Self(0);

    /// Next ID for fresh allocation. Server-side use; clients should
    /// not construct IDs out of band.
    #[inline]
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "action#{}", self.0)
    }
}

/// Server-assigned plan ID. u64, same wire layout as [`ActionId`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct PlanId(pub u64);

impl Serialize for PlanId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for PlanId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_u64(PlanIdVisitor)
    }
}

struct PlanIdVisitor;

impl<'de> serde::de::Visitor<'de> for PlanIdVisitor {
    type Value = PlanId;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a PlanId (u64)")
    }
    fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Self::Value, E> {
        Ok(PlanId(n))
    }
}

impl PlanId {
    pub const ZERO: Self = Self(0);

    #[inline]
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plan#{}", self.0)
    }
}

/// Server-assigned step index within a `Plan`. 0-based; `u32` (the
/// design targets plans of < 10⁵ steps and we want postcard-friendly
/// 4-byte encoding on the wire).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct StepId(pub u32);

impl Serialize for StepId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for StepId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_u32(StepIdVisitor)
    }
}

struct StepIdVisitor;

impl<'de> serde::de::Visitor<'de> for StepIdVisitor {
    type Value = StepId;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a StepId (u32)")
    }
    fn visit_u32<E: serde::de::Error>(self, n: u32) -> Result<Self::Value, E> {
        Ok(StepId(n))
    }
}

impl StepId {
    pub const ZERO: Self = Self(0);
}

impl std::fmt::Display for StepId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "step#{}", self.0)
    }
}

/// Server-assigned predicate ID. Created when a `Predicate` is
/// registered with the predicate engine; torn down on expiry or
/// manual cancel.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct PredicateHandle(pub u64);

impl Serialize for PredicateHandle {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for PredicateHandle {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_u64(PredicateHandleVisitor)
    }
}

struct PredicateHandleVisitor;

impl<'de> serde::de::Visitor<'de> for PredicateHandleVisitor {
    type Value = PredicateHandle;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a PredicateHandle (u64)")
    }
    fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Self::Value, E> {
        Ok(PredicateHandle(n))
    }
}

impl PredicateHandle {
    pub const ZERO: Self = Self(0);
}

impl std::fmt::Display for PredicateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pred#{}", self.0)
    }
}

/// 16-byte blake3 fingerprint of a device screen — see v3 §3.2.0.
///
/// The fingerprint is stable for the same `(a11y-hash, frame-pHash,
/// pkg:activity)` triple; cross-device stable when the same app
/// screen renders to the same tree. Used as the Memory layer key
/// (Phase 3) so the kernel can recall "what worked on this screen
/// last time".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ScreenId(pub [u8; 16]);

impl ScreenId {
    /// Empty / unknown screen fingerprint. Distinct from any real
    /// screen so equality checks don't false-match.
    pub const UNKNOWN: Self = Self([0u8; 16]);

    /// Cheap fingerprint derived from a `pkg/.activity` string only.
    /// Used by the host binary to bucket per-app Memory entries when
    /// the full a11y + frame pHash isn't available. Collisions
    /// across screens within the same app are tolerated by the
    /// Memory layer (it appends).
    #[must_use]
    pub fn from_focus(focus: &str) -> Self {
        Self::compute(b"", b"", focus)
    }

    /// Compute a screen fingerprint from the three sources of
    /// identity. Order of inputs is fixed; the resulting digest is
    /// blake3 truncated to 16 bytes.
    #[must_use]
    pub fn compute(a11y_hash: &[u8], frame_phash: &[u8], pkg_activity: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(a11y_hash.len() as u32).to_le_bytes());
        hasher.update(a11y_hash);
        hasher.update(&(frame_phash.len() as u32).to_le_bytes());
        hasher.update(frame_phash);
        hasher.update(&(pkg_activity.len() as u32).to_le_bytes());
        hasher.update(pkg_activity.as_bytes());
        // Truncate 32-byte digest to 16 bytes for the fingerprint.
        // 128 bits is well over the threshold for screen-identity
        // collision safety on a per-device basis (v3 §3.2.0).
        let mut out = [0u8; 16];
        out.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self(out)
    }

    /// Hex-encoded form (32 chars) for log lines and the
    /// `Memory` SQLite key. Lowercase, no separator.
    #[must_use]
    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl std::fmt::Display for ScreenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_wraps_at_u64_max() {
        let id = ActionId(u64::MAX);
        assert_eq!(id.next(), ActionId(0));
    }

    #[test]
    fn plan_id_wraps_at_u64_max() {
        let id = PlanId(u64::MAX);
        assert_eq!(id.next(), PlanId(0));
    }

    #[test]
    fn step_id_zero_display() {
        assert_eq!(StepId(0).to_string(), "step#0");
        assert_eq!(StepId(42).to_string(), "step#42");
    }

    #[test]
    fn display_format_is_stable() {
        // Wire-stable: these labels appear in log lines and trace
        // exports; lock them in.
        assert_eq!(ActionId(1).to_string(), "action#1");
        assert_eq!(PlanId(2).to_string(), "plan#2");
        assert_eq!(PredicateHandle(3).to_string(), "pred#3");
    }

    #[test]
    fn screen_id_is_16_bytes() {
        assert_eq!(std::mem::size_of::<ScreenId>(), 16);
    }

    #[test]
    fn screen_id_unknown_is_all_zero() {
        assert_eq!(ScreenId::UNKNOWN.0, [0u8; 16]);
    }

    #[test]
    fn screen_id_compute_is_deterministic() {
        let a = ScreenId::compute(b"a11y-v1", b"phash-v1", "com.foo/.Main");
        let b = ScreenId::compute(b"a11y-v1", b"phash-v1", "com.foo/.Main");
        assert_eq!(a, b);
        assert_ne!(a, ScreenId::UNKNOWN);
    }

    #[test]
    fn screen_id_changes_when_any_input_changes() {
        let base = ScreenId::compute(b"a11y", b"ph", "com.foo/.Main");
        assert_ne!(
            base,
            ScreenId::compute(b"a11y-mut", b"ph", "com.foo/.Main"),
            "a11y change flips the fingerprint"
        );
        assert_ne!(
            base,
            ScreenId::compute(b"a11y", b"ph-mut", "com.foo/.Main"),
            "phash change flips the fingerprint"
        );
        assert_ne!(
            base,
            ScreenId::compute(b"a11y", b"ph", "com.foo/.Other"),
            "package change flips the fingerprint"
        );
    }

    #[test]
    fn screen_id_hex_is_32_chars() {
        let id = ScreenId::compute(b"x", b"y", "p/.a");
        assert_eq!(id.hex().len(), 32);
        assert!(id.hex().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn screen_id_hex_display_matches() {
        let id = ScreenId::compute(b"x", b"y", "p/.a");
        assert_eq!(format!("{id}"), id.hex());
    }

    #[test]
    fn ids_postcard_round_trip() {
        // postcard uses varint for u64/u32, so the exact wire
        // size depends on the magnitude. We assert compactness
        // (≤ 10 B for a 64-bit value full of high-bit bytes,
        // ≤ 5 B for a 32-bit value) and round-trip integrity.
        let a = ActionId(0x0102_0304_0506_0708);
        let bytes = postcard::to_allocvec(&a).expect("encode action id");
        assert!(bytes.len() <= 10, "ActionId too fat: {} bytes", bytes.len());
        assert!(!bytes.is_empty(), "ActionId too thin: 0 bytes");
        let decoded: ActionId = postcard::from_bytes(&bytes).expect("decode action id");
        assert_eq!(decoded, a);

        let p = PlanId(0xDEAD_BEEF_CAFE_BABE);
        let bytes = postcard::to_allocvec(&p).expect("encode plan id");
        assert!(bytes.len() <= 10, "PlanId too fat: {} bytes", bytes.len());
        assert_eq!(postcard::from_bytes::<PlanId>(&bytes).unwrap(), p);

        let s = StepId(99);
        let bytes = postcard::to_allocvec(&s).expect("encode step id");
        assert!(bytes.len() <= 5, "StepId too fat: {} bytes", bytes.len());
        assert_eq!(postcard::from_bytes::<StepId>(&bytes).unwrap(), s);

        // Small IDs save bytes — a freshly allocated ID will often
        // fit in 1–2 varint bytes, which is the whole point of
        // using a varint-aware format over fixed-width 8 bytes.
        let small = ActionId(7);
        let bytes = postcard::to_allocvec(&small).expect("encode small");
        assert_eq!(bytes.len(), 1, "ActionId(7) is a 1-byte varint");

        let sid = ScreenId::compute(b"a11y", b"ph", "p/.a");
        let bytes = postcard::to_allocvec(&sid).expect("encode screen id");
        assert_eq!(bytes.len(), 16, "ScreenId encodes as fixed 16 bytes");
        assert_eq!(postcard::from_bytes::<ScreenId>(&bytes).unwrap(), sid);
    }

    #[test]
    fn ids_are_hashable() {
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(ActionId(1));
        s.insert(ActionId(2));
        s.insert(ActionId(1));
        assert_eq!(s.len(), 2);
    }
}
