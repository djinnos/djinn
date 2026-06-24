//! Reply loop guard to prevent runaway sessions.
pub(crate) struct LoopGuard {
    max_turns: u64,
    current_turn: u64,
}

impl LoopGuard {
    pub fn new(max_turns: u64) -> Self {
        Self {
            max_turns,
            current_turn: 0,
        }
    }
    pub fn advance(&mut self) -> bool {
        self.current_turn += 1;
        self.current_turn <= self.max_turns
    }
    pub fn turn_count(&self) -> u64 {
        self.current_turn
    }
}
