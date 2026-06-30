//! Error alias used across the agent API.

/// Convenience alias — every agent function returns
/// `Result<T, Box<dyn Error + Send + Sync>>` so callers can downcast
/// to whichever concrete error type they need.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;