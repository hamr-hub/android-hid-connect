//! `MockLlmAdapter` — file-based deterministic LLM adapter for tests.
//!
//! Per grill session (ADR 0007 hybrid + PRD-B's "MockLlmAdapter
//! wiring" requirement). Loads `Plan` JSON fixtures from
//! a directory; cycles through them on each `plan()` call.
//!
//! **No network I/O.** `std::fs::read_to_string` only.
//!
//! **Determinism guarantee**: same `Observation` + same fixture
//! file = same `PlanMessage` output. CI can replay tests
//! byte-identically.

use std::path::{Path, PathBuf};

use crate::observation::Observation;
use crate::plan::Plan;

/// Trait abstraction for "an LLM". `LlmAdapter::plan(obs)` returns
/// the next `PlanMessage` (or a `Plan::abort` to terminate the
/// session). Real cloud adapters (Anthropic / OpenAI) implement
/// this trait in Phase 5.3; `MockLlmAdapter` is the simulator
/// reference impl.
pub trait LlmAdapter: Send {
    /// Generate the next plan for the given observation. Pure
    /// function: same `obs` + same adapter state = same output.
    fn plan(&mut self, obs: &Observation) -> Plan;

    /// Reset internal state (e.g. for replay tests).
    fn reset(&mut self);

    /// Human-readable identifier for the adapter (used in
    /// audit logs).
    fn name(&self) -> &str;
}

/// Loads `Plan` JSON fixtures from a directory; cycles through
/// them on each `plan()` call. **Deterministic.**
pub struct MockLlmAdapter {
    /// Path to the fixture directory (for diagnostic messages).
    dir: PathBuf,
    /// Pre-loaded fixtures in stable order.
    fixtures: Vec<Plan>,
    /// Cursor into `fixtures` (modulo length).
    cursor: usize,
    /// Counter for `plan()` calls.
    call_count: u64,
}

impl MockLlmAdapter {
    /// Load all `*.json` files in `dir` as `Plan` fixtures.
    ///
    /// # Panics
    /// Panics on IO error, JSON parse error, or schema violation.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self::try_new(dir).expect("MockLlmAdapter::try_new failed")
    }

    /// Fallible variant of `new`.
    pub fn try_new(dir: impl AsRef<Path>) -> Result<Self, MockLlmError> {
        let dir = dir.as_ref().to_path_buf();
        let mut fixtures = Vec::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| MockLlmError::Io {
            path: dir.clone(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| MockLlmError::Io {
                path: dir.clone(),
                source: e.into(),
            })?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| MockLlmError::Io {
                path: path.clone(),
                source: e,
            })?;
            let plan: Plan = crate::json::parse_and_validate(&bytes).map_err(|e| {
                MockLlmError::Schema {
                    path: path.clone(),
                    source: e,
                }
            })?;
            fixtures.push(plan);
        }
        fixtures.sort_by_key(|p| p.id.0);
        if fixtures.is_empty() {
            return Err(MockLlmError::Empty(dir));
        }
        Ok(Self {
            dir,
            fixtures,
            cursor: 0,
            call_count: 0,
        })
    }

    /// Number of loaded fixtures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fixtures.len()
    }

    /// True if no fixtures are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty()
    }

    /// Path to the fixture directory (for diagnostics).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many times `plan()` has been called.
    #[must_use]
    pub fn call_count(&self) -> u64 {
        self.call_count
    }
}

impl LlmAdapter for MockLlmAdapter {
    fn plan(&mut self, _obs: &Observation) -> Plan {
        self.call_count += 1;
        if self.fixtures.is_empty() {
            return Plan::new(vec![]);
        }
        let plan = self.fixtures[self.cursor % self.fixtures.len()].clone();
        self.cursor = (self.cursor + 1) % self.fixtures.len();
        plan
    }

    fn reset(&mut self) {
        self.cursor = 0;
        self.call_count = 0;
    }

    fn name(&self) -> &str {
        "MockLlmAdapter"
    }
}

/// Errors from `MockLlmAdapter::try_new`.
#[derive(Debug, thiserror::Error)]
pub enum MockLlmError {
    #[error("I/O error reading fixture directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no `*.json` fixtures found in {0}")]
    Empty(PathBuf),
    #[error("fixture {path} failed schema validation: {source}")]
    Schema {
        path: PathBuf,
        #[source]
        source: crate::json::JsonError,
    },
}

impl std::fmt::Debug for MockLlmAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockLlmAdapter")
            .field("dir", &self.dir)
            .field("fixtures", &self.fixtures.len())
            .field("cursor", &self.cursor)
            .field("call_count", &self.call_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::PlanId;

    fn sample_plan(id: u64) -> Plan {
        Plan {
            version: "0.1.0".into(),
            id: PlanId(id),
            steps: vec![],
            abort_on_error: true,
            checkpoint_every: 0,
        }
    }

    // Smoke tests on the trait shape (no filesystem).
    // Filesystem-based tests are deferred to `tests/mock_llm_fs.rs`
    // once a `tempfile` dep is added.

    #[test]
    fn try_new_empty_dir_yields_empty_error() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ai-mock-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let r = MockLlmAdapter::try_new(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(r, Err(MockLlmError::Empty(_))));
    }

    #[test]
    fn try_new_fails_on_invalid_json_in_dir() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ai-mock-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), b"not json").unwrap();
        let r = MockLlmAdapter::try_new(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(r, Err(MockLlmError::Schema { .. }) | Err(MockLlmError::Io { .. })),
            "expected Schema or Io error, got: {r:?}"
        );
    }

    #[test]
    fn try_new_succeeds_with_one_valid_fixture() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ai-mock-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = serde_json::to_vec(&sample_plan(42)).unwrap();
        std::fs::write(dir.join("plan.json"), bytes).unwrap();
        let r = MockLlmAdapter::try_new(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut adapter = r.expect("valid fixture");
        assert_eq!(adapter.len(), 1);
        let p = adapter.plan(&Observation::default());
        assert_eq!(p.id, PlanId(42));
    }

    #[test]
    fn plan_cycles_through_fixtures() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ai-mock-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), serde_json::to_vec(&sample_plan(0)).unwrap()).unwrap();
        std::fs::write(dir.join("b.json"), serde_json::to_vec(&sample_plan(1)).unwrap()).unwrap();
        let mut adapter = MockLlmAdapter::new(&dir);
        let obs = Observation::default();
        let p0 = adapter.plan(&obs);
        let p1 = adapter.plan(&obs);
        let p2 = adapter.plan(&obs);
        let p3 = adapter.plan(&obs);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(p0.id, PlanId(0));
        assert_eq!(p1.id, PlanId(1));
        assert_eq!(p2.id, PlanId(0)); // cycled
        assert_eq!(p3.id, PlanId(1));
    }

    #[test]
    fn reset_clears_call_count() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ai-mock-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.json"), serde_json::to_vec(&sample_plan(0)).unwrap()).unwrap();
        let mut adapter = MockLlmAdapter::new(&dir);
        let _ = adapter.plan(&Observation::default());
        let _ = adapter.plan(&Observation::default());
        assert_eq!(adapter.call_count(), 2);
        adapter.reset();
        assert_eq!(adapter.call_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
