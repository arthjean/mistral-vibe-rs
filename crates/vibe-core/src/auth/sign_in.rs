//! The browser sign-in state machine.
//!
//! Reference `vibe/setup/auth/browser_sign_in.py` and
//! `browser_sign_in_gateway.py`: a PKCE `S256` attempt is created against the
//! console, the operator finishes it in a browser, and a poll loop waits for
//! the exchange token that buys the API key. The service owns the cadence and
//! the error taxonomy; the gateway behind [`SignInGateway`] owns the wire.
//!
//! The clock, the sleep, the browser opener and the verifier source all come
//! through [`SignInRuntime`], which is what lets the parity corpus drive the
//! whole state machine with no network, no browser and no real time, exactly
//! as the reference's tests inject them through constructor arguments.

use std::fmt;
use std::future::Future;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::platform::BackgroundCommand as _;
use crate::time::UtcTimestamp;

/// Seconds between polls. The reference polls at 3.0 with no backoff, and the
/// deviation from RFC 8628's 5-second recommendation is deliberate: a
/// different interval issues a different number of requests over the same
/// window, which is a measurable divergence.
pub const POLL_INTERVAL_SECONDS: f64 = 3.0;
/// The third consecutive poll failure stops the attempt.
pub const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 3;
/// The only challenge method the protocol speaks, per RFC 7636 §4.2.
pub const CODE_CHALLENGE_METHOD: &str = "S256";

// --------------------------------------------------------------------------
// The error taxonomy
// --------------------------------------------------------------------------

/// The eleven terminal conditions a sign-in can end in. The values are the
/// wire vocabulary the reference declares; nothing may invent a twelfth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInErrorCode {
    StartFailed,
    PollFailed,
    UnknownState,
    ExchangeFailed,
    MissingApiKey,
    MissingExchangeToken,
    Expired,
    Denied,
    ProviderError,
    TimedOut,
    OpenBrowserFailed,
}

impl SignInErrorCode {
    /// Every code, in the reference's declaration order.
    pub const ALL: [Self; 11] = [
        Self::StartFailed,
        Self::PollFailed,
        Self::UnknownState,
        Self::ExchangeFailed,
        Self::MissingApiKey,
        Self::MissingExchangeToken,
        Self::Expired,
        Self::Denied,
        Self::ProviderError,
        Self::TimedOut,
        Self::OpenBrowserFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartFailed => "start_failed",
            Self::PollFailed => "poll_failed",
            Self::UnknownState => "unknown_state",
            Self::ExchangeFailed => "exchange_failed",
            Self::MissingApiKey => "missing_api_key",
            Self::MissingExchangeToken => "missing_exchange_token",
            Self::Expired => "expired",
            Self::Denied => "denied",
            Self::ProviderError => "provider_error",
            Self::TimedOut => "timed_out",
            Self::OpenBrowserFailed => "open_browser_failed",
        }
    }

    /// This port's own sentence for the condition. `NOTICE` forbids shipping
    /// the reference's prose, so each run is written originally against the
    /// same directive and the parity replay holds it permanently unequal to
    /// the reference digest.
    pub fn message(self) -> &'static str {
        match self {
            Self::StartFailed => "The browser sign-in could not be started.",
            Self::PollFailed => "The browser sign-in status could not be read.",
            Self::UnknownState => {
                "The browser sign-in reported a state this build does not recognize."
            }
            Self::ExchangeFailed => "The browser sign-in could not be exchanged for an API key.",
            Self::MissingApiKey => "The browser sign-in exchange returned no API key.",
            Self::MissingExchangeToken => {
                "The browser sign-in finished, but setup could not complete it."
            }
            Self::Expired => "The browser sign-in expired before it finished.",
            Self::Denied => "The browser sign-in was refused in the browser.",
            Self::ProviderError => "The browser sign-in failed on the provider's side.",
            Self::TimedOut => "The browser sign-in ran out of time.",
            Self::OpenBrowserFailed => "No browser could be opened for the sign-in.",
        }
    }
}

impl fmt::Display for SignInErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A terminal sign-in failure: one of the eleven codes and a sentence safe to
/// show. The message never carries a credential, an exchange token, a code
/// verifier or a full server-supplied URL, which is what lets every diagnostic
/// print it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} [{code}]")]
pub struct SignInError {
    pub code: SignInErrorCode,
    message: String,
}

impl SignInError {
    pub fn new(code: SignInErrorCode) -> Self {
        Self {
            code,
            message: code.message().to_owned(),
        }
    }

    /// A failure carrying a sentence other than the code's own, which the
    /// protocol needs exactly once: a provider error repeats the sentence the
    /// server sent.
    pub fn with_message(code: SignInErrorCode, message: String) -> Self {
        Self { code, message }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<SignInErrorCode> for SignInError {
    fn from(code: SignInErrorCode) -> Self {
        Self::new(code)
    }
}

// --------------------------------------------------------------------------
// PKCE
// --------------------------------------------------------------------------

/// 64 random bytes as unpadded base64url: 512 bits of entropy, above the
/// 256-bit floor of RFC 7636 §7.1, drawn entirely from the unreserved
/// character set. `None` only when the OS random source is unavailable.
pub fn generate_code_verifier() -> Option<String> {
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

/// The unpadded base64url encoding of the SHA-256 of the verifier's ASCII
/// bytes, per RFC 7636 §4.2.
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

// --------------------------------------------------------------------------
// The gateway contract
// --------------------------------------------------------------------------

/// What the console answered when the process was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInProcess {
    pub process_id: String,
    pub sign_in_url: String,
    pub poll_url: String,
    pub expires_at: UtcTimestamp,
}

/// One poll answer. The status stays the wire string so the service owns the
/// vocabulary check, exactly as the reference's service matches the literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInPoll {
    pub status: String,
    pub exchange_token: Option<String>,
    pub message: Option<String>,
}

/// The three endpoints of the sign-in protocol, plus the close the service
/// forwards. [`super::sign_in_http::HttpSignInGateway`] is the production
/// implementation; the parity replay scripts one.
pub trait SignInGateway {
    fn create_process(
        &mut self,
        code_challenge: &str,
    ) -> impl Future<Output = Result<SignInProcess, SignInError>> + Send;

    fn poll(
        &mut self,
        poll_url: &str,
    ) -> impl Future<Output = Result<SignInPoll, SignInError>> + Send;

    fn exchange(
        &mut self,
        process_id: &str,
        exchange_token: &str,
        code_verifier: &str,
    ) -> impl Future<Output = Result<String, SignInError>> + Send;

    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

// --------------------------------------------------------------------------
// The runtime contract
// --------------------------------------------------------------------------

/// The ambient effects the service needs: a clock, a sleep, a browser opener
/// and a verifier source. Production uses [`SystemSignInRuntime`]; tests and
/// the corpus replay script every one of them.
pub trait SignInRuntime {
    fn now(&mut self) -> UtcTimestamp;

    fn sleep(&mut self, seconds: f64) -> impl Future<Output = ()> + Send;

    /// `Ok(false)` and `Err(_)` both mean no browser opened; the reference
    /// collapses the raised and the refused opener into the same code.
    fn open_browser(&mut self, url: &str) -> io::Result<bool>;

    fn code_verifier(&mut self) -> Result<String, SignInError> {
        generate_code_verifier().ok_or_else(|| SignInError::new(SignInErrorCode::StartFailed))
    }
}

/// The production runtime: the system clock, the tokio timer and the system
/// browser.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSignInRuntime;

impl SignInRuntime for SystemSignInRuntime {
    fn now(&mut self) -> UtcTimestamp {
        UtcTimestamp::now()
    }

    async fn sleep(&mut self, seconds: f64) {
        if seconds > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
        }
    }

    fn open_browser(&mut self, url: &str) -> io::Result<bool> {
        Ok(open_system_browser(url))
    }
}

/// Launches the system browser detached: the child's stdio is discarded so it
/// can never draw over the terminal, and the spawn is not waited on so a
/// launcher that blocks never stalls the sign-in. `false` when the URL is not
/// one [`openable_browser_url`] accepts or no candidate launcher could be
/// spawned; either way the caller still holds the URL to print.
pub fn open_system_browser(url: &str) -> bool {
    openable_browser_url(url)
        && browser_launch_candidates()
            .iter()
            .any(|(program, arguments)| launch_detached(program, arguments, url).is_ok())
}

/// Whether `url` may be handed to the system opener: an `http` or `https`
/// URL only.
///
/// The reference passes any URL to `webbrowser.open`, which opens every
/// scheme. A URL can come from a remote MCP server, so this port refuses to
/// let one launch a `file:` or custom-scheme handler and leaves it printed
/// for the operator instead.
#[must_use]
pub fn openable_browser_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "https" | "http"))
}

/// The programs that open a URL on this host, each with the arguments that
/// precede the URL, in the order they are tried.
///
/// The URL is always one argument of its own and no candidate is a command
/// interpreter, so nothing in it (`&`, `|`, `^`, `%VAR%`) is ever parsed as
/// shell syntax. On Windows `url.dll`'s `FileProtocolHandler` hands it to the
/// shell's URL association, as the reference's `os.startfile` does.
#[must_use]
pub fn browser_launch_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(windows) {
        &[("rundll32.exe", &["url.dll,FileProtocolHandler"])]
    } else {
        &[("xdg-open", &[]), ("gio", &["open"])]
    }
}

pub(crate) fn launch_detached(program: &str, arguments: &[&str], url: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .in_background()
        .args(arguments)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the launcher in the background so it neither blocks the flow nor
    // lingers as a zombie; if the thread cannot spawn, the child is left for
    // process exit to collect.
    let _ = std::thread::Builder::new()
        .name("browser-launch-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

// --------------------------------------------------------------------------
// The service
// --------------------------------------------------------------------------

/// The four forward steps of a healthy attempt, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInStatus {
    OpeningBrowser,
    WaitingForBrowserSignIn,
    Exchanging,
    Completed,
}

impl SignInStatus {
    pub const ALL: [Self; 4] = [
        Self::OpeningBrowser,
        Self::WaitingForBrowserSignIn,
        Self::Exchanging,
        Self::Completed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpeningBrowser => "opening_browser",
            Self::WaitingForBrowserSignIn => "waiting_for_browser_sign_in",
            Self::Exchanging => "exchanging",
            Self::Completed => "completed",
        }
    }
}

/// What the service tells a watching UI: the attempt identity once, then each
/// status as it is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInEvent {
    AttemptStarted {
        sign_in_url: String,
        expires_at: UtcTimestamp,
    },
    StatusChanged(SignInStatus),
}

/// A started attempt: everything a caller needs to finish it later, including
/// the sign-in URL that stays retrievable for manual use after the browser
/// opened or failed to.
#[derive(Clone)]
pub struct SignInAttempt {
    pub process_id: String,
    pub sign_in_url: String,
    pub poll_url: String,
    pub expires_at: UtcTimestamp,
    code_verifier: String,
}

impl fmt::Debug for SignInAttempt {
    /// The verifier is the PKCE secret, so no derived output may carry it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignInAttempt")
            .field("process_id", &self.process_id)
            .field("sign_in_url", &self.sign_in_url)
            .field("poll_url", &self.poll_url)
            .field("expires_at", &self.expires_at)
            .field("code_verifier", &"<redacted>")
            .finish()
    }
}

pub type SignInEventSink<'a> = &'a mut (dyn FnMut(SignInEvent) + Send);

/// The sign-in state machine. Reference `BrowserSignInService`: create the
/// process, open the browser, poll until a terminal answer, exchange the
/// token, and stop with one of the eleven codes on any other path.
pub struct SignInService<G, R> {
    gateway: G,
    runtime: R,
}

impl<G: SignInGateway, R: SignInRuntime> SignInService<G, R> {
    pub fn new(gateway: G, runtime: R) -> Self {
        Self { gateway, runtime }
    }

    /// Closes the gateway. A cancelled attempt drops its future and calls
    /// this, so the connection never outlives the flow that opened it.
    pub async fn close(&mut self) {
        self.gateway.close().await;
    }

    /// Gives the gateway and runtime back, which is how the parity replay
    /// reads the scripted call log after a drive.
    pub fn into_parts(self) -> (G, R) {
        (self.gateway, self.runtime)
    }

    pub async fn start_attempt(&mut self) -> Result<SignInAttempt, SignInError> {
        let verifier = self.runtime.code_verifier()?;
        let challenge = code_challenge(&verifier);
        let process = self.gateway.create_process(&challenge).await?;
        Ok(SignInAttempt {
            process_id: process.process_id,
            sign_in_url: process.sign_in_url,
            poll_url: process.poll_url,
            expires_at: process.expires_at,
            code_verifier: verifier,
        })
    }

    /// Finishes an attempt whose browser the caller already handled, which is
    /// what the delegated editor flow does.
    pub async fn complete_attempt(
        &mut self,
        attempt: &SignInAttempt,
    ) -> Result<String, SignInError> {
        self.finish_attempt(attempt, &mut |_| {}).await
    }

    /// The whole flow: start, announce, open the browser, wait and exchange.
    pub async fn authenticate(
        &mut self,
        on_event: SignInEventSink<'_>,
    ) -> Result<String, SignInError> {
        let attempt = self.start_attempt().await?;
        on_event(SignInEvent::AttemptStarted {
            sign_in_url: attempt.sign_in_url.clone(),
            expires_at: attempt.expires_at,
        });
        on_event(SignInEvent::StatusChanged(SignInStatus::OpeningBrowser));
        self.open_browser_or_fail(&attempt.sign_in_url)?;
        self.finish_attempt(&attempt, on_event).await
    }

    async fn finish_attempt(
        &mut self,
        attempt: &SignInAttempt,
        on_event: SignInEventSink<'_>,
    ) -> Result<String, SignInError> {
        on_event(SignInEvent::StatusChanged(
            SignInStatus::WaitingForBrowserSignIn,
        ));
        let exchange_token = self.wait_for_completion(attempt).await?;
        on_event(SignInEvent::StatusChanged(SignInStatus::Exchanging));
        let api_key = self
            .gateway
            .exchange(&attempt.process_id, &exchange_token, &attempt.code_verifier)
            .await?;
        on_event(SignInEvent::StatusChanged(SignInStatus::Completed));
        Ok(api_key)
    }

    async fn wait_for_completion(
        &mut self,
        attempt: &SignInAttempt,
    ) -> Result<String, SignInError> {
        let mut consecutive_poll_failures = 0u32;
        while self.runtime.now() < attempt.expires_at {
            let poll = match self.gateway.poll(&attempt.poll_url).await {
                Ok(poll) => poll,
                Err(error) => {
                    if error.code != SignInErrorCode::PollFailed {
                        return Err(error);
                    }
                    consecutive_poll_failures += 1;
                    if consecutive_poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                        return Err(error);
                    }
                    self.sleep_until_next_poll_or_timeout(attempt.expires_at)
                        .await?;
                    continue;
                }
            };
            consecutive_poll_failures = 0;
            match poll.status.as_str() {
                "pending" => {
                    self.sleep_until_next_poll_or_timeout(attempt.expires_at)
                        .await?;
                }
                "completed" => {
                    return poll
                        .exchange_token
                        .filter(|token| !token.is_empty())
                        .ok_or_else(|| SignInError::new(SignInErrorCode::MissingExchangeToken));
                }
                "expired" => return Err(SignInError::new(SignInErrorCode::Expired)),
                "denied" => return Err(SignInError::new(SignInErrorCode::Denied)),
                "error" => {
                    let message = poll
                        .message
                        .filter(|message| !message.is_empty())
                        .unwrap_or_else(|| SignInErrorCode::ProviderError.message().to_owned());
                    return Err(SignInError::with_message(
                        SignInErrorCode::ProviderError,
                        message,
                    ));
                }
                _ => return Err(SignInError::new(SignInErrorCode::UnknownState)),
            }
        }
        Err(SignInError::new(SignInErrorCode::TimedOut))
    }

    async fn sleep_until_next_poll_or_timeout(
        &mut self,
        expires_at: UtcTimestamp,
    ) -> Result<(), SignInError> {
        let remaining = expires_at.seconds_since(self.runtime.now());
        if remaining <= 0.0 {
            return Err(SignInError::new(SignInErrorCode::TimedOut));
        }
        self.runtime
            .sleep(POLL_INTERVAL_SECONDS.min(remaining))
            .await;
        Ok(())
    }

    fn open_browser_or_fail(&mut self, sign_in_url: &str) -> Result<(), SignInError> {
        match self.runtime.open_browser(sign_in_url) {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(SignInError::new(SignInErrorCode::OpenBrowserFailed)),
        }
    }
}
