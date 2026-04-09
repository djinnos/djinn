//! Djinn-native conversation compaction.
//!
//! When the accumulated input token count reaches 80% of the model's context
//! window, `compact_conversation` summarises the conversation via the LLM and
//! replaces the in-memory `Conversation` with a compact representation. The
//! original messages are persisted to the `session_messages` table before the
//! replacement so nothing is lost.

mod policy;
mod prompts;
mod summarizer;

pub(crate) use policy::{compact_conversation, needs_compaction};
pub(crate) use prompts::CompactionContext;
