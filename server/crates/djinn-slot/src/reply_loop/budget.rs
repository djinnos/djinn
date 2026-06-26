//! Reply loop budget tracking.
pub(crate) struct SessionBudget {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub max_context_tokens: u64,
}

impl SessionBudget {
    pub fn new(max_context_tokens: u64) -> Self {
        Self {
            tokens_in: 0,
            tokens_out: 0,
            max_context_tokens,
        }
    }
    pub fn add_tokens(&mut self, tokens_in: u64, tokens_out: u64) {
        self.tokens_in += tokens_in;
        self.tokens_out += tokens_out;
    }
    pub fn is_exhausted(&self) -> bool {
        self.tokens_in + self.tokens_out > self.max_context_tokens
    }
}
