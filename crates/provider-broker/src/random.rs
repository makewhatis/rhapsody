//! Injected randomness.
//!
//! Token minting needs 32 OS-CSPRNG bytes per attempt and retries at most eight times on a digest
//! collision; registration needs a domain-separated 128-bit session id (design §4.1). Both go
//! through [`RandomSource`] so `OS randomness failure` and `collision exhaustion` are deterministic
//! in tests via [`ScriptedRandom`].

use std::collections::VecDeque;
use std::sync::Mutex;

/// The random source could not produce bytes. The caller maps this to
/// [`BrokerError::RandomSourceFailure`](crate::BrokerError::RandomSourceFailure); no capability
/// becomes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RandomError;

/// A cryptographically secure byte source.
pub trait RandomSource: Send + Sync + 'static {
    /// Fill `dest` completely with random bytes.
    fn fill(&self, dest: &mut [u8]) -> Result<(), RandomError>;
}

/// The production source: the OS CSPRNG via `getrandom`.
#[derive(Debug, Default)]
pub struct OsRandom;

impl OsRandom {
    /// Construct the OS-CSPRNG source.
    pub fn new() -> Self {
        Self
    }
}

impl RandomSource for OsRandom {
    fn fill(&self, dest: &mut [u8]) -> Result<(), RandomError> {
        getrandom::getrandom(dest).map_err(|_| RandomError)
    }
}

/// A deterministic test source.
///
/// Each queued entry is consumed by one `fill` call. When the queue is empty the source either
/// produces a unique counter-seeded value (the default, so tests get distinct tokens without
/// scripting every byte) or fails, depending on [`ScriptedRandom::with_fallback`].
#[derive(Debug)]
pub struct ScriptedRandom {
    state: Mutex<ScriptedState>,
}

#[derive(Debug)]
struct ScriptedState {
    queue: VecDeque<ScriptedFill>,
    counter: u64,
    fallback: bool,
}

#[derive(Debug)]
enum ScriptedFill {
    Bytes(Vec<u8>),
    Fail,
}

impl ScriptedRandom {
    /// A source that generates unique counter-seeded bytes once its queue is empty.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ScriptedState {
                queue: VecDeque::new(),
                counter: 0,
                fallback: true,
            }),
        }
    }

    /// Choose whether an empty queue yields counter-seeded bytes (`true`) or a failure (`false`).
    pub fn with_fallback(self, fallback: bool) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.fallback = fallback;
        }
        self
    }

    /// Queue the exact bytes the next `fill` call should return.
    pub fn push_bytes(&self, bytes: impl Into<Vec<u8>>) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.push_back(ScriptedFill::Bytes(bytes.into()));
        }
    }

    /// Queue a failure for the next `fill` call.
    pub fn push_failure(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.queue.push_back(ScriptedFill::Fail);
        }
    }
}

impl Default for ScriptedRandom {
    fn default() -> Self {
        Self::new()
    }
}

impl RandomSource for ScriptedRandom {
    fn fill(&self, dest: &mut [u8]) -> Result<(), RandomError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match state.queue.pop_front() {
            Some(ScriptedFill::Bytes(bytes)) => {
                let copy = bytes.len().min(dest.len());
                dest[..copy].copy_from_slice(&bytes[..copy]);
                dest[copy..].fill(0);
                Ok(())
            }
            Some(ScriptedFill::Fail) => Err(RandomError),
            None if state.fallback => {
                let seed = state.counter;
                state.counter = state.counter.wrapping_add(1);
                let word = seed.to_be_bytes();
                for (index, byte) in dest.iter_mut().enumerate() {
                    *byte = *word.get(index).unwrap_or(&0);
                }
                Ok(())
            }
            None => Err(RandomError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_bytes_are_returned_verbatim() {
        let source = ScriptedRandom::new();
        source.push_bytes(vec![1, 2, 3, 4]);
        let mut buffer = [0u8; 4];
        source.fill(&mut buffer).expect("scripted bytes");
        assert_eq!(buffer, [1, 2, 3, 4]);
    }

    #[test]
    fn scripted_failure_is_reported() {
        let source = ScriptedRandom::new().with_fallback(false);
        source.push_failure();
        let mut buffer = [0u8; 4];
        assert_eq!(source.fill(&mut buffer), Err(RandomError));
    }

    #[test]
    fn fallback_produces_distinct_values() {
        let source = ScriptedRandom::new();
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        source.fill(&mut first).expect("first");
        source.fill(&mut second).expect("second");
        assert_ne!(first, second);
    }

    #[test]
    fn empty_queue_without_fallback_fails() {
        let source = ScriptedRandom::new().with_fallback(false);
        let mut buffer = [0u8; 4];
        assert_eq!(source.fill(&mut buffer), Err(RandomError));
    }
}
