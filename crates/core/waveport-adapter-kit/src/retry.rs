//! Whether to try again, and when.

use core::time::Duration;

use waveport_ports::error::{PortError, RetryClass};

/// What an HTTP status that is not a success means for trying again.
#[must_use]
pub const fn class_of_status(status: u16) -> RetryClass {
    match status {
        401 | 403 => RetryClass::Auth,
        408 | 504 => RetryClass::Timeout,
        429 => RetryClass::RateLimited,
        // The request was fine and the server was not. 409 and 425 ask
        // for the same request again.
        409 | 425 | 500..=599 => RetryClass::Transient,
        _ => RetryClass::Permanent,
    }
}

/// How long a `Retry-After` header asks to wait, when it gives a number
/// of seconds. The other form, a date, is not read: no clock is at hand
/// here to measure it against.
#[must_use]
pub fn retry_after(value: &str) -> Option<Duration> {
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && (0.0..=86_400.0).contains(&seconds))
        .then(|| Duration::from_secs_f64(seconds))
}

/// The failure an HTTP status stands for. `detail` is what the provider
/// said about it, with any key struck out.
#[must_use]
pub fn error_of_status(status: u16, retry_after_header: Option<&str>, detail: &str) -> PortError {
    let detail: String = detail.chars().take(MAX_DETAIL).collect();
    let message = if detail.trim().is_empty() {
        format!("the provider answered {status}")
    } else {
        format!("the provider answered {status}: {}", detail.trim())
    };
    let class = class_of_status(status);
    PortError {
        class,
        retry_after: retry_after_header
            .and_then(retry_after)
            .filter(|_| matches!(class, RetryClass::RateLimited | RetryClass::Transient)),
        message: message.into(),
    }
}

/// The most of a provider's words that are kept in an error.
const MAX_DETAIL: usize = 300;

/// What the close code of a WebSocket means for trying again, when the
/// provider and not this side closed it.
#[must_use]
pub const fn class_of_close(code: u16) -> RetryClass {
    match code {
        // What was sent is not what the server takes.
        1002 | 1003 | 1007 | 1008 | 1009 | 1010 => RetryClass::Permanent,
        // Everything else is the server's own trouble or its wish to be
        // left: normal closure, going away, internal error, restart, try
        // again later, and the codes providers make up.
        _ => RetryClass::Transient,
    }
}

/// How long to wait before each new try.
///
/// The wait doubles from `first` up to `longest`. A share of it is taken
/// off at random, so that many sessions that failed together do not come
/// back together. The randomness is the caller's: this type has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The wait before the second try.
    pub first: Duration,
    /// The longest wait.
    pub longest: Duration,
}

impl Backoff {
    /// Waits from 100 ms up to 5 s.
    pub const DEFAULT: Self = Self {
        first: Duration::from_millis(100),
        longest: Duration::from_secs(5),
    };

    /// The wait after `failures` tries have failed, with `random` any
    /// number: between half the doubled wait and all of it.
    #[must_use]
    pub fn wait(&self, failures: u32, random: u32) -> Duration {
        let doubled = self
            .first
            .saturating_mul(
                1_u32
                    .checked_shl(failures.saturating_sub(1))
                    .unwrap_or(u32::MAX),
            )
            .min(self.longest);
        let half = doubled / 2;
        // A share of the other half, from none of it to all of it.
        let share = half.mul_f64(f64::from(random) / f64::from(u32::MAX));
        half + share
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use waveport_ports::error::RetryClass;

    use super::{Backoff, class_of_close, class_of_status, error_of_status, retry_after};

    #[test]
    fn a_status_says_whether_to_try_again() {
        let cases = [
            (400, RetryClass::Permanent),
            (401, RetryClass::Auth),
            (403, RetryClass::Auth),
            (404, RetryClass::Permanent),
            (408, RetryClass::Timeout),
            (409, RetryClass::Transient),
            (422, RetryClass::Permanent),
            (429, RetryClass::RateLimited),
            (500, RetryClass::Transient),
            (502, RetryClass::Transient),
            (503, RetryClass::Transient),
            (504, RetryClass::Timeout),
            (529, RetryClass::Transient),
        ];
        for (status, class) in cases {
            assert_eq!(class_of_status(status), class, "{status}");
        }
    }

    #[test]
    fn a_wait_is_read_when_it_is_a_number_of_seconds() {
        assert_eq!(retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(retry_after(" 0.5 "), Some(Duration::from_millis(500)));
        assert_eq!(retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(retry_after("-1"), None);
        assert_eq!(retry_after("inf"), None);
        assert_eq!(
            retry_after("1e9"),
            None,
            "longer than a day is not believed"
        );
    }

    #[test]
    fn an_error_keeps_the_status_the_wait_and_a_little_of_what_was_said() {
        let error = error_of_status(429, Some("3"), " Rate limit reached \n");
        assert_eq!(error.class, RetryClass::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_secs(3)));
        assert_eq!(
            &*error.message,
            "the provider answered 429: Rate limit reached"
        );

        let error = error_of_status(401, Some("3"), "");
        assert_eq!(error.class, RetryClass::Auth);
        assert_eq!(error.retry_after, None, "waiting does not mend a key");
        assert_eq!(&*error.message, "the provider answered 401");

        let long = "x".repeat(10_000);
        assert!(error_of_status(500, None, &long).message.len() < 400);
    }

    #[test]
    fn a_close_code_says_whether_to_try_again() {
        for code in [1000, 1001, 1006, 1011, 1012, 1013, 4000] {
            assert_eq!(class_of_close(code), RetryClass::Transient, "{code}");
        }
        for code in [1002, 1003, 1007, 1008, 1009] {
            assert_eq!(class_of_close(code), RetryClass::Permanent, "{code}");
        }
    }

    #[test]
    fn waits_double_up_to_the_longest_and_are_spread() {
        let backoff = Backoff::DEFAULT;
        let longest = |failures| backoff.wait(failures, u32::MAX).as_millis();
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7, 8].map(longest),
            [100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000]
        );
        // Never less than half.
        assert_eq!(backoff.wait(3, 0), Duration::from_millis(200));
        let middle = backoff.wait(3, u32::MAX / 2).as_micros();
        assert!((299_990..=300_010).contains(&middle), "{middle}");
        // However many tries, nothing overflows.
        assert_eq!(longest(u32::MAX), 5_000);
        assert_eq!(longest(0), 100);
    }
}
