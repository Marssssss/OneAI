//! ScopeState — isolated state for sub-agent execution (MVI/Redux pattern).

use oneai_core::{GlobalState, MemoryEntry, Reduction};

/// ScopeState — isolated state for a sub-agent during parallel execution.
///
/// Each sub-agent clones a read-only snapshot of the global memory,
/// runs in its own private Sandbox Scope, and accumulates reductions
/// to merge back into the global state after completion.
#[derive(Debug, Clone)]
pub struct ScopeState {
    /// Read-only clone of global memory at the start of execution.
    pub global_memory: Vec<MemoryEntry>,

    /// Private sandbox — mutations made by this sub-agent.
    pub local_sandbox: Vec<MemoryEntry>,

    /// Pending reductions to merge back into global state.
    pub pending_reductions: Vec<Reduction>,
}

impl ScopeState {
    /// Create a new ScopeState from the current global state.
    pub fn from_global(global: &GlobalState) -> Self {
        Self {
            global_memory: global.memory.clone(),
            local_sandbox: Vec::new(),
            pending_reductions: Vec::new(),
        }
    }

    /// Add a memory entry to the local sandbox.
    pub fn add_local_memory(&mut self, entry: MemoryEntry) {
        self.local_sandbox.push(entry);
    }

    /// Record a reduction to merge back into global state.
    pub fn add_reduction(&mut self, reduction: Reduction) {
        self.pending_reductions.push(reduction);
    }

    /// Get all pending reductions for merging.
    pub fn reductions(&self) -> &[Reduction] {
        &self.pending_reductions
    }
}

/// Default StateReducer — merges reductions into global state using simple append/replace.
pub struct DefaultStateReducer;

impl oneai_core::traits::StateReducer for DefaultStateReducer {
    fn reduce(
        &self,
        global: &mut GlobalState,
        reductions: Vec<Reduction>,
    ) -> oneai_core::error::Result<()> {
        for reduction in reductions {
            match reduction {
                Reduction::AppendMemory { entry } => {
                    global.memory.push(entry);
                }
                Reduction::UpdateContext { key, value } => {
                    global.context.insert(key, value);
                }
                Reduction::SetResult { step_id, result } => {
                    global.step_results.insert(step_id, result);
                }
            }
        }
        Ok(())
    }
}