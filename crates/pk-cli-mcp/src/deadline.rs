//! How long a write may still go out: DESIGN.md §1.9, "A late write sends nothing".
//!
//! `<bin> mcp --http` answers a call at [`CALL_LIMIT`] and does not cancel it, so what the call
//! was waiting on, a keychain prompt or a lock, can end long after anyone is listening. A
//! toolset starts a [`SendDeadline`] when a call begins and checks it right before each send:
//!
//! ```
//! use pk_cli_mcp::{Clock, SendDeadline};
//! use std::time::Duration;
//!
//! const SEND_DEADLINE: Duration = Duration::from_secs(60);
//!
//! fn switch(clock: &dyn Clock) -> Result<(), pk_cli_core::CliError> {
//!     let deadline = SendDeadline::start(clock, SEND_DEADLINE);
//!     // ...the credential, the device list, anything else that can wait...
//!     deadline.check()?;
//!     // ...the command...
//!     Ok(())
//! }
//! ```
//!
//! Nothing here reads a real clock but [`SystemClock`], which does not exist on
//! `wasm32-unknown-unknown`: `Instant::now` and `SystemTime::now` both panic there.

use std::fmt;
use std::time::Duration;

use pk_cli_core::CliError;

/// How long `<bin> mcp --http` waits for a call before answering the Worker with an error (Claude
/// gives a remote tool call 240 s in all). The call runs on to its end, unobserved, so a write's
/// deadline leaves room for its read-back inside this.
pub const CALL_LIMIT: Duration = Duration::from_secs(120);

/// The two clocks a deadline reads, as a seam so its tests need no real sleep
/// (`conformance::ManualClock`, feature `test-util`).
pub trait Clock {
    /// Time since a fixed point, on a clock that never moves back. It can stop while the machine
    /// sleeps.
    fn monotonic(&self) -> Duration;
    /// Time since the Unix epoch. It runs on through a sleep, and can be set back.
    fn wall(&self) -> Duration;
}

/// The machine's own clocks. Native only.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[cfg(not(target_arch = "wasm32"))]
impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(std::time::Instant::now).elapsed()
    }

    fn wall(&self) -> Duration {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
    }
}

/// When a call began, on both clocks, and how long after that its command may still go.
///
/// A monotonic clock stops while a Mac sleeps, so a prompt answered the next morning reads as
/// seconds later on it; the wall clock runs on. A wall clock that moved back says nothing, which
/// leaves the monotonic one. Elapsed time is whichever of the two says more.
#[derive(Clone, Copy)]
pub struct SendDeadline<'c> {
    clock: &'c dyn Clock,
    monotonic: Duration,
    wall: Duration,
    limit: Duration,
}

impl<'c> SendDeadline<'c> {
    /// Start the time now. Call it first thing in the call, before anything that can wait.
    pub fn start(clock: &'c dyn Clock, limit: Duration) -> Self {
        Self {
            clock,
            monotonic: clock.monotonic(),
            wall: clock.wall(),
            limit,
        }
    }

    /// How long the call has been running. A write's later limits, such as when a read-back may
    /// last start, compare against this.
    pub fn elapsed(&self) -> Duration {
        let monotonic = self.clock.monotonic().saturating_sub(self.monotonic);
        let wall = self.clock.wall().saturating_sub(self.wall);
        monotonic.max(wall)
    }

    /// Call right before each send. At the limit or past it, an error saying nothing was sent,
    /// which a caller can act on as it stands: return it, send nothing, read nothing back.
    ///
    /// The kind is [`CliError::ConfirmationRequired`], not `Upstream`: the wait may have been on
    /// this machine with the provider never contacted, and `Upstream`'s remedy is to try again in
    /// a minute. A retry nobody watches is the late write this exists to stop, so the remedy here
    /// is the user's fresh say-so, and what a client asks before each write gets it.
    pub fn check(&self) -> Result<(), CliError> {
        if self.elapsed() < self.limit {
            return Ok(());
        }
        Err(CliError::ConfirmationRequired(format!(
            "nothing was sent: this call took over {} s to reach the command, on a prompt, a \
             lock, a slow request or a machine asleep, and whoever asked may have stopped \
             waiting. Call it again only if the user still wants the change.",
            self.limit.as_secs()
        )))
    }
}

impl fmt::Debug for SendDeadline<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendDeadline")
            .field("elapsed", &self.elapsed())
            .field("limit", &self.limit)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::ManualClock;
    use crate::{Server, ToolError};

    const LIMIT: Duration = Duration::from_secs(60);
    const JUST_UNDER: Duration = Duration::from_millis(59_999);
    const HOUR: Duration = Duration::from_secs(60 * 60);

    #[test]
    fn a_call_inside_its_limit_may_send() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        deadline.check().unwrap();
        clock.advance(JUST_UNDER);
        deadline.check().unwrap();
        assert_eq!(deadline.elapsed(), JUST_UNDER);
    }

    #[test]
    fn a_call_at_its_limit_sends_nothing_and_says_so() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        clock.advance(LIMIT);
        let err = deadline.check().unwrap_err();
        assert_eq!(err.exit_code(), 6, "{err:?}");
        let text = err.to_string();
        assert!(text.contains("nothing was sent"), "{text}");
        assert!(text.contains("over 60 s"), "{text}");
    }

    /// The wait may have been on this machine, and a retry nobody watches is what this stops, so
    /// what the model reads must not blame the provider or send it back in a minute.
    #[test]
    fn a_late_write_tells_the_model_to_ask_and_not_to_retry() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        clock.advance(LIMIT);
        let server = Server::new("demo", "0.0.0", "Demo", "demo");
        let text = ToolError::from(deadline.check().unwrap_err()).render(&server);
        assert!(text.starts_with("confirmation_required: "), "{text}");
        assert!(text.contains("nothing was sent"), "{text}");
        assert!(text.contains("only if the user still wants"), "{text}");
        assert!(text.contains("needs the user's confirmation"), "{text}");
        assert!(!text.contains("provider failed"), "{text}");
        assert!(!text.contains("in a minute"), "{text}");
    }

    /// `Instant` stands still while a Mac sleeps, so only the wall clock sees this night pass.
    #[test]
    fn a_machine_that_slept_through_the_wait_sends_nothing() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        clock.sleep(8 * HOUR);
        assert_eq!(deadline.elapsed(), 8 * HOUR);
        assert!(deadline.check().is_err());
    }

    /// Daylight saving, a corrected clock: an hour back must not buy an hour of waiting.
    #[test]
    fn a_wall_clock_set_back_leaves_the_monotonic_one() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        clock.set_wall_back(HOUR);
        assert_eq!(deadline.elapsed(), Duration::ZERO);
        deadline.check().unwrap();
        clock.advance(LIMIT);
        assert!(deadline.check().is_err());
    }

    #[test]
    fn a_limit_of_nothing_never_sends() {
        let clock = ManualClock::new();
        assert!(SendDeadline::start(&clock, Duration::ZERO).check().is_err());
    }

    #[test]
    fn a_copy_shares_the_start() {
        let clock = ManualClock::new();
        let deadline = SendDeadline::start(&clock, LIMIT);
        clock.advance(JUST_UNDER);
        let copy = deadline;
        assert_eq!(copy.elapsed(), JUST_UNDER);
    }

    #[test]
    fn the_real_clocks_read_without_a_wait() {
        let deadline = SendDeadline::start(&SystemClock, HOUR);
        deadline.check().unwrap();
        assert!(deadline.elapsed() < HOUR);
        assert!(SystemClock.wall() > Duration::from_secs(1_577_836_800));
        assert!(SystemClock.monotonic() <= SystemClock.monotonic());
    }
}
