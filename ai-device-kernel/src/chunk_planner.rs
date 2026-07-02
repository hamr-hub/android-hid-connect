//! `ChunkPlanner` — N=4 chunk 切分 + 末尾 observe 强约束。
//!
//! Per grill ADR 0003 (planning-granularity-is-per-chunk) +
//! PRD-A AC-4 / AC-5. The LLM is asked to emit `Plan` whose
//! `steps: Vec<PlanStep>` is auto-segmented into chunks of at most
//! [`MAX_CHUNK_SIZE`] atomics, **with an `observe` step at the end
//! of every chunk** (the LLM re-plans at chunk boundaries, per
//! grill session Q1.1 / ADR 0002 "flow control is LLM's").
//!
//! **No IO, no kernel calls** — pure planning logic.
//! `bin/adk` and `LlmLoop` consume this module.

use serde::{Deserialize, Serialize};
use schemars::JsonSchema;

use crate::action::Action;
use crate::plan::{Plan, PlanStep};

/// Default chunk size per grill ADR 0003 (N=4).
pub const MAX_CHUNK_SIZE: usize = 4;

/// Error from `validate_chunk`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    /// Chunk has more than [`MAX_CHUNK_SIZE`] atomics.
    #[error("chunk has {count} atomics, max is {max}")]
    TooLarge { count: usize, max: usize },
    /// Chunk does not end with an observe atomic
    /// (`Action::DumpObservation`).
    #[error("chunk must end with an observe atomic (DumpObservation)")]
    MissingObserve,
    /// Chunk is empty (zero atomics). Not always an error, but
    /// useless for LLM re-planning.
    #[error("chunk is empty")]
    Empty,
}

/// One planned chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Chunk {
    /// Index of this chunk in the parent plan.
    pub index: u32,
    /// Total chunks in the parent plan.
    pub total: u32,
    /// The plan steps in this chunk. Length ≤ [`MAX_CHUNK_SIZE`].
    pub steps: Vec<PlanStep>,
}

/// Plan a chunk-size sequence for a total atomic count.
///
/// Examples (with `MAX_CHUNK_SIZE = 4`):
/// - 0 → `vec![]`
/// - 1 → `vec![1]`
/// - 4 → `vec![4]`
/// - 7 → `vec![4, 3]`
/// - 10 → `vec![4, 4, 2]`
pub fn plan_chunk_size(total_atomics: usize) -> Vec<usize> {
    if total_atomics == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut remaining = total_atomics;
    while remaining > 0 {
        let take = remaining.min(MAX_CHUNK_SIZE);
        chunks.push(take);
        remaining -= take;
    }
    chunks
}

/// Validate a single chunk. Enforces:
/// - chunk length ≤ [`MAX_CHUNK_SIZE`]
/// - last step is an observe atomic
pub fn validate_chunk(chunk: &[PlanStep]) -> Result<(), ChunkError> {
    if chunk.is_empty() {
        return Err(ChunkError::Empty);
    }
    if chunk.len() > MAX_CHUNK_SIZE {
        return Err(ChunkError::TooLarge {
            count: chunk.len(),
            max: MAX_CHUNK_SIZE,
        });
    }
    let last = chunk.last().expect("non-empty checked above");
    if !is_observe_action(&last.action) {
        return Err(ChunkError::MissingObserve);
    }
    Ok(())
}

/// True if the action is an observe atomic (re-plan trigger).
fn is_observe_action(action: &Action) -> bool {
    matches!(action, Action::DumpObservation { .. })
}

/// Split a `Plan` into chunks of at most `MAX_CHUNK_SIZE` atomics
/// each, with the last action of each chunk being an observe
/// atomic. **If the input plan does not end with an observe, this
/// function appends one** (per ADR 0003's hard requirement that
/// chunks terminate in observe).
///
/// Returns an empty `Vec` if the plan has zero steps.
pub fn split_into_chunks(plan: &Plan) -> Vec<Chunk> {
    if plan.steps.is_empty() {
        return Vec::new();
    }
    // Ensure plan ends with observe (append if missing).
    let mut steps = plan.steps.clone();
    let ends_with_observe = steps
        .last()
        .map(|s| is_observe_action(&s.action))
        .unwrap_or(false);
    if !ends_with_observe {
        steps.push(PlanStep::new(Action::DumpObservation {
            components: vec![],
            deadline_ms: 100,
        }));
    }
    let sizes = plan_chunk_size(steps.len());
    let total = sizes.len() as u32;
    let mut chunks = Vec::with_capacity(sizes.len());
    let mut offset = 0;
    for (i, n) in sizes.iter().enumerate() {
        let chunk_steps = steps[offset..offset + n].to_vec();
        offset += n;
        chunks.push(Chunk {
            index: i as u32,
            total,
            steps: chunk_steps,
        });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::ids::PlanId;

    fn tap_at(x: i32, y: i32) -> PlanStep {
        PlanStep::new(Action::Tap {
            x,
            y,
            deadline_ms: 100,
        })
    }

    fn observe_step() -> PlanStep {
        PlanStep::new(Action::DumpObservation {
            components: vec![],
            deadline_ms: 100,
        })
    }

    fn sample_plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            version: "0.1.0".into(),
            id: PlanId(0),
            steps,
            abort_on_error: true,
            checkpoint_every: 0,
        }
    }

    #[test]
    fn plan_chunk_size_zero() {
        assert_eq!(plan_chunk_size(0), Vec::<usize>::new());
    }

    #[test]
    fn plan_chunk_size_one() {
        assert_eq!(plan_chunk_size(1), vec![1]);
    }

    #[test]
    fn plan_chunk_size_exact_four() {
        assert_eq!(plan_chunk_size(4), vec![4]);
    }

    #[test]
    fn plan_chunk_size_seven() {
        assert_eq!(plan_chunk_size(7), vec![4, 3]);
    }

    #[test]
    fn plan_chunk_size_ten() {
        assert_eq!(plan_chunk_size(10), vec![4, 4, 2]);
    }

    #[test]
    fn plan_chunk_size_eleven() {
        assert_eq!(plan_chunk_size(11), vec![4, 4, 3]);
    }

    #[test]
    fn plan_chunk_size_twelve() {
        assert_eq!(plan_chunk_size(12), vec![4, 4, 4]);
    }

    #[test]
    fn validate_chunk_accepts_four_with_observe() {
        let chunk = vec![tap_at(0, 0), tap_at(1, 1), tap_at(2, 2), observe_step()];
        validate_chunk(&chunk).expect("valid chunk");
    }

    #[test]
    fn validate_chunk_rejects_too_large() {
        let chunk = vec![tap_at(0, 0); 5];
        let r = validate_chunk(&chunk);
        assert!(matches!(r, Err(ChunkError::TooLarge { count: 5, max: 4 })));
    }

    #[test]
    fn validate_chunk_rejects_missing_observe() {
        let chunk = vec![tap_at(0, 0), tap_at(1, 1), tap_at(2, 2)];
        let r = validate_chunk(&chunk);
        assert!(matches!(r, Err(ChunkError::MissingObserve)));
    }

    #[test]
    fn validate_chunk_rejects_empty() {
        let chunk: Vec<PlanStep> = vec![];
        let r = validate_chunk(&chunk);
        assert!(matches!(r, Err(ChunkError::Empty)));
    }

    #[test]
    fn split_into_chunks_appends_observe_when_missing() {
        let plan = sample_plan(vec![tap_at(0, 0), tap_at(1, 1)]);
        let chunks = split_into_chunks(&plan);
        assert_eq!(chunks.len(), 1, "2 atomics → 1 chunk (size=2 < 4, observe appended)");
        assert_eq!(chunks[0].steps.len(), 3, "tap + tap + appended observe");
        assert!(is_observe_action(&chunks[0].steps.last().unwrap().action));
    }

    #[test]
    fn split_into_chunks_splits_at_max_chunk_size() {
        let plan = sample_plan(vec![
            tap_at(0, 0), tap_at(1, 1), tap_at(2, 2), tap_at(3, 3),
            tap_at(4, 4), tap_at(5, 5), tap_at(6, 6),
        ]);
        let chunks = split_into_chunks(&plan);
        assert_eq!(chunks.len(), 2, "7 atomics + observe → 2 chunks");
        assert_eq!(chunks[0].steps.len(), 4);
        assert_eq!(chunks[1].steps.len(), 4, "3 + 1 observe = 4");
    }

    #[test]
    fn split_into_chunks_assigns_indices() {
        let plan = sample_plan(vec![tap_at(0, 0); 7]);
        let chunks = split_into_chunks(&plan);
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].total, 2);
        assert_eq!(chunks[1].index, 1);
        assert_eq!(chunks[1].total, 2);
    }

    #[test]
    fn split_into_chunks_preserves_existing_observe() {
        // 4 atomics where the last is already observe — split should
        // not double-append.
        let plan = sample_plan(vec![
            tap_at(0, 0), tap_at(1, 1), tap_at(2, 2), observe_step(),
        ]);
        let chunks = split_into_chunks(&plan);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].steps.len(), 4, "no double-append");
    }

    #[test]
    fn split_into_chunks_empty_plan() {
        let plan = sample_plan(vec![]);
        let chunks = split_into_chunks(&plan);
        assert!(chunks.is_empty());
    }

    // unused; keep import live for future predicates
}
