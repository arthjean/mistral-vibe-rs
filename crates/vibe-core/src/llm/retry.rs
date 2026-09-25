//! When a failed request is tried again, how long the backend waits, and how
//! calls are spaced after a rate limit.
//!
//! Two schedules coexist, as in the reference. The generic backend retries
//! with `async_retry` (`vibe/core/utils/retry.py`): 500 ms doubling to a 60 s
//! cap plus 50 ms per attempt, a server's `Retry-After` honored up to the cap,
//! for as long as the elapsed budget allows. The Mistral backend inherits its
//! client's schedule (`mistralai/client/utils/retries.py`): 500 ms growing by
//! 1.5 to a 30 s cap plus up to a second of jitter, a positive `Retry-After`
//! honored as it is, and one more attempt as long as the budget is not yet
//! overrun.
//!
//! [`AdaptivePacer`] is `vibe/core/utils/pacing.py`: no pacing until the first
//! rate limit, then a minimum gap between call starts that doubles with every
//! further one and shrinks by half a second once a minute passes without any.
//!
//! Every wait goes through a [`Clock`], so a test can run a budget of minutes
//! in no time and read back each wait it asked for.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::error::{BackendError, BackendErrorSource, BackendFailure};

pub type SleepFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Which wait a sleep is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepKind {
    /// A backoff before a retry.
    Retry,
    /// The pacer spacing two calls.
    Pacer,
}

/// Time as the retry machinery reads it.
pub trait Clock: Send + Sync {
    /// Seconds on a monotonic scale.
    fn monotonic(&self) -> f64;
    /// The wall clock, which a `Retry-After` date is measured against.
    fn now(&self) -> SystemTime;
    fn sleep(&self, seconds: f64, kind: SleepKind) -> SleepFuture;
    /// A draw from `[0, 1)`, the jitter the Mistral client adds to a backoff.
    fn jitter(&self) -> f64;
}

/// The process clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, seconds: f64, _kind: SleepKind) -> SleepFuture {
        let duration = Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX);
        Box::pin(tokio::time::sleep(duration))
    }

    fn jitter(&self) -> f64 {
        let mut bytes = [0_u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            return 0.5;
        }
        // 53 random bits make a uniform double in [0, 1).
        #[allow(clippy::cast_precision_loss)]
        let fraction = (u64::from_le_bytes(bytes) >> 11) as f64 / (1_u64 << 53) as f64;
        fraction
    }
}

/// What a client can render about a retry. Reference `RetryCategory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryCategory {
    RateLimited,
    ServerError,
    TimedOut,
    Connection,
    Unknown,
}

impl RetryCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::TimedOut => "timed_out",
            Self::Connection => "connection",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn for_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimited,
            408 => Self::TimedOut,
            500.. => Self::ServerError,
            _ => Self::Unknown,
        }
    }
}

/// Why a request is being retried: a category and the raw detail, `HTTP 503`
/// or the transport failure's name. Reference `RetryReason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryReason {
    pub category: RetryCategory,
    pub detail: String,
}

impl RetryReason {
    #[must_use]
    pub fn for_status(status: u16) -> Self {
        Self {
            category: RetryCategory::for_status(status),
            detail: format!("HTTP {status}"),
        }
    }

    /// The reason a failure is retried for.
    #[must_use]
    pub fn for_failure(failure: &BackendFailure) -> Self {
        match failure {
            BackendFailure::Backend(error) => Self::for_backend_error(error),
            BackendFailure::Local(failure) => Self {
                category: RetryCategory::Unknown,
                detail: failure.kind.reference_class().to_owned(),
            },
            BackendFailure::ResponsesStream(error) => match error.status {
                Some(status) => Self::for_status(status),
                None => Self {
                    category: RetryCategory::Unknown,
                    detail: "OpenAIResponsesStreamError".to_owned(),
                },
            },
        }
    }

    fn for_backend_error(error: &BackendError) -> Self {
        match (&error.source, error.status) {
            (BackendErrorSource::Request(kind), _) => Self {
                category: if kind.is_timeout() {
                    RetryCategory::TimedOut
                } else if kind.is_retryable() {
                    RetryCategory::Connection
                } else {
                    RetryCategory::Unknown
                },
                detail: kind.name().to_owned(),
            },
            (_, Some(status)) => Self::for_status(status),
            (_, None) => Self {
                category: RetryCategory::Unknown,
                detail: "OpenAIResponsesStreamError".to_owned(),
            },
        }
    }
}

/// Told about every retry while the request still waits on it.
pub trait RetryObserver: Send + Sync {
    fn retrying(&self, reason: &RetryReason);
}

/// An observer that listens to nothing.
pub struct NoRetryObserver;

impl RetryObserver for NoRetryObserver {
    fn retrying(&self, _reason: &RetryReason) {}
}

/// The statuses the generic backend retries.
pub const GENERIC_RETRYABLE_STATUSES: &[u16] = &[408, 409, 425, 429, 500, 502, 503, 504, 529];

/// The statuses the Mistral client retries.
pub const MISTRAL_RETRYABLE_STATUSES: &[u16] = &[429, 500, 502, 503, 504];

/// Whether the generic backend tries a failed request again: a retryable
/// status, a Responses stream error that maps to one, or a transport failure
/// it expects to clear.
#[must_use]
pub fn generic_is_retryable(failure: &BackendFailure) -> bool {
    let error = match failure {
        BackendFailure::Backend(error) => error,
        BackendFailure::Local(_) => return false,
        BackendFailure::ResponsesStream(error) => {
            return error
                .status
                .is_some_and(|status| GENERIC_RETRYABLE_STATUSES.contains(&status));
        }
    };
    match &error.source {
        BackendErrorSource::Request(kind) => kind.is_retryable(),
        BackendErrorSource::Status | BackendErrorSource::Client => error
            .status
            .is_some_and(|status| GENERIC_RETRYABLE_STATUSES.contains(&status)),
        BackendErrorSource::Stream => error
            .status
            .is_some_and(|status| GENERIC_RETRYABLE_STATUSES.contains(&status)),
    }
}

/// A `Retry-After` value in seconds, as the generic backend reads it: ASCII
/// digits, or an HTTP date measured from `now`, a past one reading as zero.
#[must_use]
pub fn generic_retry_after(value: &str, now: SystemTime) -> Option<f64> {
    let value = super::sse::python_strip(value);
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<f64>().ok();
    }
    let at = parse_http_date(value)?;
    Some(seconds_between(now, at).max(0.0))
}

/// The generic backend's wait before retry number `attempt` (counting from
/// zero). Reference `_next_delay`.
#[must_use]
pub fn next_delay(
    retry_after: Option<f64>,
    attempt: u32,
    delay: f64,
    factor: f64,
    cap: f64,
) -> f64 {
    if let Some(retry_after) = retry_after {
        return retry_after.min(cap);
    }
    if delay > 0.0 && factor > 1.0 && cap > 0.0 {
        let saturated_after = ((cap / delay).ln() / factor.ln()).floor();
        if f64::from(attempt) > saturated_after {
            return cap;
        }
    }
    let exponential = if delay > 0.0 {
        delay * factor.powf(f64::from(attempt))
    } else {
        0.0
    };
    // Written out rather than fused, so the rounding is the reference's.
    (exponential + 0.05 * f64::from(attempt)).min(cap)
}

/// The generic backend's own schedule: 500 ms doubling to a minute.
pub const GENERIC_DELAY: f64 = 0.5;
pub const GENERIC_FACTOR: f64 = 2.0;
pub const GENERIC_CAP: f64 = 60.0;

/// A `Retry-After` value in milliseconds, as the Mistral client reads it: any
/// number of seconds, or an HTTP date, a past one reading as zero.
#[must_use]
pub fn mistral_retry_after_millis(value: &str, now: SystemTime) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    if let Some(seconds) = python_float(value) {
        #[allow(clippy::cast_possible_truncation)]
        return Some((seconds * 1000.0).round_ties_even() as i64);
    }
    let at = parse_http_date(value)?;
    #[allow(clippy::cast_possible_truncation)]
    Some((seconds_between(now, at).max(0.0) * 1000.0).round_ties_even() as i64)
}

/// The Mistral client's wait before retry number `retries`, in seconds.
#[must_use]
pub fn mistral_delay(retry_after_millis: Option<i64>, retries: u32, jitter: f64) -> f64 {
    if let Some(millis) = retry_after_millis.filter(|millis| *millis > 0) {
        #[allow(clippy::cast_precision_loss)]
        return millis as f64 / 1000.0;
    }
    let sleep = 0.5 * 1.5f64.powf(f64::from(retries)) + jitter;
    sleep.min(30.0)
}

/// `float()` over a header value: surrounding whitespace, a sign, digits with
/// an optional fraction and exponent, `inf` and `nan`.
fn python_float(value: &str) -> Option<f64> {
    let trimmed = super::sse::python_strip(value);
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed
        .strip_prefix(['+', '-'])
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    let special = matches!(body.as_str(), "inf" | "infinity" | "nan");
    let numeric = !body.is_empty()
        && body
            .chars()
            .all(|character| character.is_ascii_digit() || "._e+-".contains(character));
    if !(special || numeric) {
        return None;
    }
    trimmed.replace('_', "").parse::<f64>().ok()
}

fn seconds_between(from: SystemTime, to: SystemTime) -> f64 {
    match to.duration_since(from) {
        Ok(ahead) => ahead.as_secs_f64(),
        Err(behind) => -behind.duration().as_secs_f64(),
    }
}

/// An RFC 2822 or RFC 9110 date, as `email.utils.parsedate_to_datetime`
/// accepts one: an optional weekday, day, month name, year, time, and a zone
/// (a numeric offset or `GMT`/`UT`/`UTC`/`Z`; none reads as UTC).
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let cleaned = value.replace(',', " ");
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    if words
        .first()
        .is_some_and(|word| word.chars().all(char::is_alphabetic) && month(word).is_none())
    {
        words.remove(0);
    }
    if words.len() < 4 {
        return None;
    }
    let (day, month_number) = match (words[0].parse::<u32>(), month(words[1])) {
        (Ok(day), Some(month)) => (day, month),
        _ => match (month(words[0]), words[1].parse::<u32>()) {
            (Some(month), Ok(day)) => (day, month),
            _ => return None,
        },
    };
    let mut year: i64 = words[2].parse().ok()?;
    if year < 100 {
        year += if year > 68 { 1900 } else { 2000 };
    }
    let mut clock = words[3].split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let offset_seconds = match words.get(4) {
        None => 0,
        Some(zone) => zone_offset(zone)?,
    };
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 61 {
        return None;
    }
    let days = days_from_civil(year, month_number, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds;
    let magnitude = Duration::from_secs(seconds.unsigned_abs());
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(magnitude)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(magnitude)
    }
}

fn month(word: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = word.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|name| lower.starts_with(name))
        .and_then(|index| u32::try_from(index + 1).ok())
}

fn zone_offset(zone: &str) -> Option<i64> {
    match zone.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" => return Some(0),
        "EST" => return Some(-5 * 3_600),
        "EDT" => return Some(-4 * 3_600),
        "CST" => return Some(-6 * 3_600),
        "CDT" => return Some(-5 * 3_600),
        "MST" => return Some(-7 * 3_600),
        "MDT" => return Some(-6 * 3_600),
        "PST" => return Some(-8 * 3_600),
        "PDT" => return Some(-7 * 3_600),
        _ => {}
    }
    let (sign, digits) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Spaces successive calls after a rate limit. Reference `AdaptivePacer`.
pub struct AdaptivePacer {
    clock: Arc<dyn Clock>,
    state: Mutex<PacerState>,
    gate: tokio::sync::Mutex<()>,
}

#[derive(Debug, Default)]
struct PacerState {
    min_interval: f64,
    last_call_at: Option<f64>,
    last_rate_limited_at: f64,
    seen_rate_limit: bool,
    rate_limited_this_call: bool,
}

const PACER_BASE_INTERVAL: f64 = 1.0;
const PACER_FACTOR: f64 = 2.0;
const PACER_MAX_INTERVAL: f64 = 60.0;
const PACER_RECOVERY_WINDOW: f64 = 60.0;
const PACER_ADDITIVE_DECREASE: f64 = 0.5;

impl AdaptivePacer {
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: Mutex::new(PacerState::default()),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    fn with_state<R>(&self, action: impl FnOnce(&mut PacerState) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        action(&mut state)
    }

    /// Waits so this call starts at least the current interval after the last.
    pub async fn acquire(&self) {
        let _gate = self.gate.lock().await;
        let wait = self.with_state(|state| {
            state.last_call_at.map_or(0.0, |last| {
                state.min_interval - (self.clock.monotonic() - last)
            })
        });
        if wait > 0.0 {
            self.clock.sleep(wait, SleepKind::Pacer).await;
        }
        let now = self.clock.monotonic();
        self.with_state(|state| {
            state.last_call_at = Some(now);
            state.rate_limited_this_call = false;
        });
    }

    pub fn on_rate_limited(&self) {
        let now = self.clock.monotonic();
        self.with_state(|state| {
            state.min_interval = if state.min_interval == 0.0 {
                PACER_BASE_INTERVAL
            } else {
                (state.min_interval * PACER_FACTOR).min(PACER_MAX_INTERVAL)
            };
            state.last_rate_limited_at = now;
            state.seen_rate_limit = true;
            state.rate_limited_this_call = true;
        });
    }

    pub fn on_success(&self) {
        self.settle(true);
    }

    pub fn on_failure(&self) {
        self.settle(false);
    }

    fn settle(&self, recover: bool) {
        let now = self.clock.monotonic();
        self.with_state(|state| {
            if !state.seen_rate_limit || state.min_interval == 0.0 {
                state.rate_limited_this_call = false;
                return;
            }
            if state.rate_limited_this_call {
                state.last_rate_limited_at = now;
                state.rate_limited_this_call = false;
                return;
            }
            if !recover || now - state.last_rate_limited_at < PACER_RECOVERY_WINDOW {
                return;
            }
            state.min_interval = (state.min_interval - PACER_ADDITIVE_DECREASE).max(0.0);
        });
    }

    /// The enforced gap between call starts, in seconds.
    #[must_use]
    pub fn min_interval(&self) -> f64 {
        self.with_state(|state| state.min_interval)
    }
}
