//! Scrcpy UHID backend — thin alias over the existing byte-exact
//! `android_hid_connect::HidSession`.

pub use android_hid_connect::HidSession as ScrcpyBackend;