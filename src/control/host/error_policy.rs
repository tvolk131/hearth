//! How the helper's job runner treats a failed step.
//!
//! Every error that escapes a job is sorted into one of three classes:
//!
//! - [`ErrorClass::Retryable`]: a read, observation or command-transport
//!   failure. When the job has no outstanding effect intent, nothing uncertain
//!   happened, so the runner retries the job with exponential backoff and only
//!   blocks after [`RETRY_BUDGET`] for one step.
//! - [`ErrorClass::Permanent`]: a deterministic refusal (validation or
//!   admission). Retrying cannot help, so the job blocks with that class.
//! - [`ErrorClass::Ambiguous`]: anything else, including every failure while an
//!   effect intent is outstanding. The job blocks for an administrator.
//!
//! Errors are unclassified by default, and an unclassified error is ambiguous:
//! only code that knows a failure is safe to repeat marks it retryable.

use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// First delay after a retryable failure.
pub const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
/// Longest delay between two attempts of one step.
pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
/// Total time one step may keep failing retryably before its job blocks.
/// Measured from the first failure, so it also covers helper downtime.
pub const RETRY_BUDGET: Duration = Duration::from_secs(30 * 60);
/// How long a stopping helper lets its in-flight step finish before it exits.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Retryable,
    Permanent,
    Ambiguous,
}

impl ErrorClass {
    fn label(self) -> &'static str {
        match self {
            Self::Retryable => "transient failure",
            Self::Permanent => "permanent refusal",
            Self::Ambiguous => "uncertain outcome",
        }
    }
}

/// An error tagged with its class. The original error stays reachable as the
/// source, so `{:#}` still prints the full chain and a wrapped
/// [`super::Crash`] is still detected.
#[derive(Debug)]
pub struct ClassifiedError {
    pub class: ErrorClass,
    source: anyhow::Error,
}

impl fmt::Display for ClassifiedError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(self.class.label())
    }
}

impl std::error::Error for ClassifiedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

fn classified(class: ErrorClass, error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError {
        class,
        source: error,
    })
}

pub fn retryable(error: impl Into<anyhow::Error>) -> anyhow::Error {
    classified(ErrorClass::Retryable, error.into())
}

pub fn permanent(error: impl Into<anyhow::Error>) -> anyhow::Error {
    classified(ErrorClass::Permanent, error.into())
}

pub fn ambiguous(error: impl Into<anyhow::Error>) -> anyhow::Error {
    classified(ErrorClass::Ambiguous, error.into())
}

/// The outermost class in the chain, if any. An outer classification wins, so
/// wrapping a retryable read in [`ambiguous`] makes the whole failure ambiguous.
pub fn class_of(error: &anyhow::Error) -> Option<ErrorClass> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>())
        .map(|classified| classified.class)
}

/// Durable retry bookkeeping for one job. Stored in the journal so that the
/// budget keeps running across helper restarts.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RetryState {
    /// The step (or pre-step phase) that keeps failing.
    pub step: String,
    /// Wall-clock milliseconds of this step's first retryable failure.
    pub first_failure_ms: u64,
    /// Retryable failures of this step so far.
    pub attempts: u32,
    /// Wall-clock milliseconds before which the job is not run again.
    pub next_attempt_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: RETRY_INITIAL_DELAY,
            max_delay: RETRY_MAX_DELAY,
            budget: RETRY_BUDGET,
        }
    }
}

/// What to do after a retryable failure.
#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    Retry(RetryState),
    Exhausted { attempts: u32, elapsed: Duration },
}

impl RetryPolicy {
    /// Delay after the `attempts`-th consecutive failure: doubling from
    /// `initial_delay`, capped at `max_delay`.
    pub fn delay(&self, attempts: u32) -> Duration {
        let factor = 1_u32
            .checked_shl(attempts.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial_delay
            .checked_mul(factor)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }

    /// Record one more retryable failure of `step` at `now_ms`. A failure of a
    /// different step than the recorded one starts a fresh budget.
    pub fn record(&self, previous: Option<&RetryState>, step: &str, now_ms: u64) -> RetryDecision {
        let (first_failure_ms, attempts) = match previous {
            Some(state) if state.step == step => (state.first_failure_ms, state.attempts + 1),
            _ => (now_ms, 1),
        };
        // A wall clock stepped backward counts as no elapsed time.
        let elapsed = Duration::from_millis(now_ms.saturating_sub(first_failure_ms));
        if elapsed >= self.budget {
            return RetryDecision::Exhausted { attempts, elapsed };
        }
        let delay = u64::try_from(self.delay(attempts).as_millis()).unwrap_or(u64::MAX);
        RetryDecision::Retry(RetryState {
            step: step.into(),
            first_failure_ms,
            attempts,
            next_attempt_ms: now_ms.saturating_add(delay),
        })
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Returned by the job runner when the helper is stopping between steps. The
/// job is left exactly as journaled and resumes on the next start.
#[derive(Debug)]
pub struct ShuttingDown;

impl fmt::Display for ShuttingDown {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str("helper is stopping; remaining steps resume on restart")
    }
}

impl std::error::Error for ShuttingDown {}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn backoff_doubles_from_two_seconds_and_caps_at_a_minute() {
        let policy = RetryPolicy::default();
        let delays: Vec<_> = (1..=8).map(|n| policy.delay(n).as_secs()).collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(policy.delay(u32::MAX), RETRY_MAX_DELAY);
    }

    #[test]
    fn budget_runs_from_the_first_failure_of_the_same_step() {
        let policy = RetryPolicy::default();
        let RetryDecision::Retry(first) = policy.record(None, "vm", 1_000) else {
            panic!("first failure must retry")
        };
        assert_eq!((first.attempts, first.next_attempt_ms), (1, 3_000));
        let RetryDecision::Retry(second) = policy.record(Some(&first), "vm", 3_000) else {
            panic!("second failure must retry")
        };
        assert_eq!((second.first_failure_ms, second.attempts), (1_000, 2));
        assert_eq!(second.next_attempt_ms, 7_000);
        let late = 1_000 + RETRY_BUDGET.as_millis() as u64;
        assert_eq!(
            policy.record(Some(&second), "vm", late),
            RetryDecision::Exhausted {
                attempts: 3,
                elapsed: RETRY_BUDGET
            }
        );
        // Another step gets its own budget.
        assert!(matches!(
            policy.record(Some(&second), "volume", late),
            RetryDecision::Retry(RetryState { attempts: 1, .. })
        ));
        // A clock stepped backward does not exhaust the budget.
        assert!(matches!(
            policy.record(Some(&second), "vm", 0),
            RetryDecision::Retry(_)
        ));
    }

    #[test]
    fn the_outermost_class_wins_and_unclassified_errors_have_none() {
        assert_eq!(class_of(&anyhow!("plain")), None);
        let read = retryable(anyhow!("zfs get failed"));
        assert_eq!(class_of(&read), Some(ErrorClass::Retryable));
        let wrapped = ambiguous(read.context("reconciling a failed effect"));
        assert_eq!(class_of(&wrapped), Some(ErrorClass::Ambiguous));
        assert_eq!(
            format!("{wrapped:#}"),
            "uncertain outcome: reconciling a failed effect: transient failure: zfs get failed"
        );
        let crash = retryable(super::super::Crash);
        assert!(crash.chain().any(|cause| cause.is::<super::super::Crash>()));
    }
}
