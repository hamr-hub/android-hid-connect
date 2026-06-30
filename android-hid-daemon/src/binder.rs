//! Binder / hidden-API helpers.
//!
//! Phase 2 will fill in the actual `IBinder` calls (service lookup,
//! `transact`, parcel marshalling) needed to reach APIs like
//! `IInputManager.injectInputEvent`, `IAccessibilityManager.getRoot`,
//! `IPackageManager.getInstalledPackages`, etc.

/// Disable Android's hidden-API exemption flags so we can call
/// internal APIs that are normally blocked on production builds.
///
/// Phase 2 will replace this `unimplemented!()` with a real
/// `setHiddenApiExemptions` call via reflection on ART.
pub fn disable_hidden_api_exemptions() {
    unimplemented!("disable_hidden_api_exemptions lands in Phase 2")
}