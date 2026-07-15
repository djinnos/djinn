//! Deterministic, runner-owned primitives for smoke scenario harnesses.
//!
//! These types intentionally model scripted outcomes rather than a live provider or
//! transport. They perform no I/O, use no credentials, and never wait on wall time.

use std::collections::VecDeque;

use thiserror::Error;

/// Provider scripts and typed observable results.
pub mod provider {
    use super::{HarnessError, VecDeque};

    /// One provider response selected in FIFO order by [`MockProvider`].
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ProviderScript {
        Success {
            turn: String,
        },
        RetryableFailure {
            detail: String,
        },
        CredentialDenied {
            detail: String,
        },
        /// A stream that finishes without producing an actionable turn.
        StreamingNoTurn {
            chunks: Vec<String>,
        },
    }

    impl ProviderScript {
        pub fn success(turn: impl Into<String>) -> Self {
            Self::Success { turn: turn.into() }
        }

        pub fn retryable_failure(detail: impl Into<String>) -> Self {
            Self::RetryableFailure {
                detail: detail.into(),
            }
        }

        pub fn credential_denied(detail: impl Into<String>) -> Self {
            Self::CredentialDenied {
                detail: detail.into(),
            }
        }

        pub fn streaming_no_turn(chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
            Self::StreamingNoTurn {
                chunks: chunks.into_iter().map(Into::into).collect(),
            }
        }
    }

    /// An outcome a smoke scenario can inspect without parsing provider-specific errors.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ProviderOutcome {
        Turn { content: String },
        RetryableFailure { detail: String },
        CredentialDenied { detail: String },
        NoTurn { chunks: Vec<String> },
    }

    /// FIFO scripted provider with a monotonically increasing logical call count.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct MockProvider {
        script: VecDeque<ProviderScript>,
        calls: u64,
    }

    impl MockProvider {
        pub fn new(script: impl IntoIterator<Item = ProviderScript>) -> Self {
            Self {
                script: script.into_iter().collect(),
                calls: 0,
            }
        }

        pub fn calls(&self) -> u64 {
            self.calls
        }

        pub fn remaining(&self) -> usize {
            self.script.len()
        }

        /// Consume exactly one scripted outcome.
        pub fn execute(&mut self) -> Result<ProviderOutcome, HarnessError> {
            let call = self.calls;
            let script = self
                .script
                .pop_front()
                .ok_or(HarnessError::ScriptExhausted {
                    harness: "provider",
                    next_sequence: call,
                })?;
            self.calls += 1;
            Ok(match script {
                ProviderScript::Success { turn } => ProviderOutcome::Turn { content: turn },
                ProviderScript::RetryableFailure { detail } => {
                    ProviderOutcome::RetryableFailure { detail }
                }
                ProviderScript::CredentialDenied { detail } => {
                    ProviderOutcome::CredentialDenied { detail }
                }
                ProviderScript::StreamingNoTurn { chunks } => ProviderOutcome::NoTurn { chunks },
            })
        }
    }
}

/// A deterministic fake channel state machine with an ordered logical event history.
pub mod channel {
    use super::HarnessError;

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ChannelScript {
        Deliver { message: String },
        EmptyTurn,
        Stall,
        Crash { detail: String },
        Reconnect,
    }

    impl ChannelScript {
        pub fn deliver(message: impl Into<String>) -> Self {
            Self::Deliver {
                message: message.into(),
            }
        }

        pub fn crash(detail: impl Into<String>) -> Self {
            Self::Crash {
                detail: detail.into(),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ChannelState {
        Connected,
        Stalled,
        Crashed,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum ChannelAction {
        Delivered { message: String },
        EmptyTurn,
        Stalled,
        Crashed { detail: String },
        Reconnected,
    }

    /// A state transition stamped with injected logical sequence, never wall-clock time.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ChannelEvent {
        pub sequence: u64,
        pub action: ChannelAction,
        pub state: ChannelState,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct FakeChannel {
        state: ChannelState,
        history: Vec<ChannelEvent>,
        next_sequence: u64,
    }

    impl Default for FakeChannel {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeChannel {
        pub fn new() -> Self {
            Self {
                state: ChannelState::Connected,
                history: Vec::new(),
                next_sequence: 0,
            }
        }

        pub fn state(&self) -> ChannelState {
            self.state
        }

        pub fn history(&self) -> &[ChannelEvent] {
            &self.history
        }

        /// Apply one injected transition and append its ordered history event.
        pub fn apply(&mut self, script: ChannelScript) -> Result<&ChannelEvent, HarnessError> {
            let action = match (&self.state, script) {
                (ChannelState::Connected, ChannelScript::Deliver { message }) => {
                    ChannelAction::Delivered { message }
                }
                (ChannelState::Connected, ChannelScript::EmptyTurn) => ChannelAction::EmptyTurn,
                (ChannelState::Connected, ChannelScript::Stall) => {
                    self.state = ChannelState::Stalled;
                    ChannelAction::Stalled
                }
                (_, ChannelScript::Crash { detail }) => {
                    self.state = ChannelState::Crashed;
                    ChannelAction::Crashed { detail }
                }
                (ChannelState::Stalled | ChannelState::Crashed, ChannelScript::Reconnect) => {
                    self.state = ChannelState::Connected;
                    ChannelAction::Reconnected
                }
                (state, script) => {
                    return Err(HarnessError::InvalidChannelTransition {
                        state: *state,
                        script,
                    });
                }
            };
            let event = ChannelEvent {
                sequence: self.next_sequence,
                action,
                state: self.state,
            };
            self.next_sequence += 1;
            self.history.push(event);
            Ok(self.history.last().expect("event was pushed"))
        }

        /// Apply every supplied input in order. Empty scripts are diagnosed explicitly.
        pub fn run(
            &mut self,
            script: impl IntoIterator<Item = ChannelScript>,
        ) -> Result<&[ChannelEvent], HarnessError> {
            let mut applied = false;
            for event in script {
                applied = true;
                self.apply(event)?;
            }
            if !applied {
                return Err(HarnessError::ScriptExhausted {
                    harness: "channel",
                    next_sequence: self.next_sequence,
                });
            }
            Ok(self.history())
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HarnessError {
    #[error(
        "{harness} script exhausted before logical sequence {next_sequence}; add a scripted event"
    )]
    ScriptExhausted {
        harness: &'static str,
        next_sequence: u64,
    },
    #[error("channel cannot apply {script:?} while {state:?}; reconnect before delivering a turn")]
    InvalidChannelTransition {
        state: channel::ChannelState,
        script: channel::ChannelScript,
    },
}

#[cfg(test)]
mod tests {
    use super::{HarnessError, channel::*, provider::*};

    #[test]
    fn provider_exposes_every_scripted_mode_in_fifo_order() {
        let mut provider = MockProvider::new([
            ProviderScript::success("answer"),
            ProviderScript::retryable_failure("try again"),
            ProviderScript::credential_denied("token revoked"),
            ProviderScript::streaming_no_turn(["partial", " metadata"]),
        ]);

        assert_eq!(
            provider.execute().unwrap(),
            ProviderOutcome::Turn {
                content: "answer".into()
            }
        );
        assert_eq!(
            provider.execute().unwrap(),
            ProviderOutcome::RetryableFailure {
                detail: "try again".into()
            }
        );
        assert_eq!(
            provider.execute().unwrap(),
            ProviderOutcome::CredentialDenied {
                detail: "token revoked".into()
            }
        );
        assert_eq!(
            provider.execute().unwrap(),
            ProviderOutcome::NoTurn {
                chunks: vec!["partial".into(), " metadata".into()]
            }
        );
        assert_eq!(provider.calls(), 4);
        assert_eq!(provider.remaining(), 0);
    }

    #[test]
    fn provider_exhaustion_names_the_missing_logical_call() {
        let error = MockProvider::new([]).execute().unwrap_err();
        assert_eq!(
            error,
            HarnessError::ScriptExhausted {
                harness: "provider",
                next_sequence: 0,
            }
        );
        assert!(error.to_string().contains("add a scripted event"));
    }

    #[test]
    fn channel_records_delivery_empty_turn_stall_crash_and_reconnect_in_order() {
        let mut channel = FakeChannel::new();
        let history = channel
            .run([
                ChannelScript::deliver("first"),
                ChannelScript::EmptyTurn,
                ChannelScript::Stall,
                ChannelScript::Reconnect,
                ChannelScript::crash("socket closed"),
                ChannelScript::Reconnect,
                ChannelScript::deliver("second"),
            ])
            .unwrap();

        assert_eq!(
            history
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            history[0].action,
            ChannelAction::Delivered {
                message: "first".into()
            }
        );
        assert_eq!(history[1].action, ChannelAction::EmptyTurn);
        assert_eq!(history[2].state, ChannelState::Stalled);
        assert_eq!(history[3].action, ChannelAction::Reconnected);
        assert_eq!(history[4].state, ChannelState::Crashed);
        assert_eq!(
            history[6].action,
            ChannelAction::Delivered {
                message: "second".into()
            }
        );
        assert_eq!(channel.state(), ChannelState::Connected);
    }

    #[test]
    fn channel_repeatability_and_invalid_transitions_are_deterministic() {
        let script = [
            ChannelScript::Stall,
            ChannelScript::Reconnect,
            ChannelScript::EmptyTurn,
        ];
        let mut first = FakeChannel::new();
        let mut second = FakeChannel::new();
        first.run(script.clone()).unwrap();
        second.run(script).unwrap();
        assert_eq!(first.history(), second.history());

        first.apply(ChannelScript::Stall).unwrap();
        let error = first
            .apply(ChannelScript::deliver("while stalled"))
            .unwrap_err();
        assert_eq!(first.state(), ChannelState::Stalled);
        assert!(matches!(
            error,
            HarnessError::InvalidChannelTransition { .. }
        ));
        assert!(error.to_string().contains("reconnect"));
        assert_eq!(
            FakeChannel::new().run([]).unwrap_err().to_string(),
            "channel script exhausted before logical sequence 0; add a scripted event"
        );
    }
}
