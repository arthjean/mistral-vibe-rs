//! URLs split the way Python's `urllib.parse` splits them.
//!
//! The reference compares, validates and rewrites URLs through `urlsplit`,
//! `urlparse` and `urlunsplit`, which split on delimiters and normalize almost
//! nothing: the path keeps its dot segments and its encoding, an omitted port
//! stays omitted, and a relative reference splits as readily as an absolute
//! one. A WHATWG parser answers differently on each of those, so every
//! contract that publishes a split URL, compares its parts or hands one back
//! rebuilt goes through this port instead. Reference behavior is CPython
//! 3.12's `Lib/urllib/parse.py`, the interpreter the pinned checkout runs.

use std::fmt::Write as _;
use std::net::Ipv6Addr;

/// `_WHATWG_C0_CONTROL_OR_SPACE`: what `urlsplit` strips off the front.
const C0_CONTROL_OR_SPACE: &[char] = &[
    '\x00', '\x01', '\x02', '\x03', '\x04', '\x05', '\x06', '\x07', '\x08', '\t', '\n', '\x0b',
    '\x0c', '\r', '\x0e', '\x0f', '\x10', '\x11', '\x12', '\x13', '\x14', '\x15', '\x16', '\x17',
    '\x18', '\x19', '\x1a', '\x1b', '\x1c', '\x1d', '\x1e', '\x1f', ' ',
];

/// `uses_netloc`: the schemes `urlunsplit` writes `//` for even when the
/// network location is empty.
const USES_NETLOC: &[&str] = &[
    "",
    "ftp",
    "http",
    "gopher",
    "nntp",
    "telnet",
    "imap",
    "wais",
    "file",
    "mms",
    "https",
    "shttp",
    "snews",
    "prospero",
    "rtsp",
    "rtsps",
    "rtspu",
    "rsync",
    "svn",
    "svn+ssh",
    "sftp",
    "nfs",
    "git",
    "git+ssh",
    "ws",
    "wss",
    "itms-services",
];

/// `uses_params`: the schemes `urlparse` splits `;params` off for.
const USES_PARAMS: &[&str] = &[
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
    "sip", "sips", "mms", "sftp", "tel",
];

/// The `ValueError` `urlsplit` raises on a malformed bracketed host, or that
/// `SplitResult.port` raises on a port that is not a number in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUrl;

/// The parts `urllib.parse` splits a URL into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PyUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

impl PyUrl {
    /// `urlsplit`, minus its bracketed-host validation: the scheme
    /// lowercased, nothing else normalized.
    #[must_use]
    pub fn split(url: &str) -> Self {
        let cleaned: String = url
            .trim_start_matches(C0_CONTROL_OR_SPACE)
            .chars()
            .filter(|character| !matches!(character, '\t' | '\r' | '\n'))
            .collect();
        let mut rest = cleaned.as_str();
        let mut parts = Self::default();
        if let Some(colon) = rest.find(':') {
            let candidate = &rest[..colon];
            if candidate
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
                && candidate
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
            {
                parts.scheme = candidate.to_ascii_lowercase();
                rest = &rest[colon + 1..];
            }
        }
        if let Some(after) = rest.strip_prefix("//") {
            let end = after.find(['/', '?', '#']).unwrap_or(after.len());
            parts.netloc = after[..end].to_owned();
            rest = &after[end..];
        }
        if let Some(hash) = rest.find('#') {
            parts.fragment = rest[hash + 1..].to_owned();
            rest = &rest[..hash];
        }
        if let Some(question) = rest.find('?') {
            parts.query = rest[question + 1..].to_owned();
            rest = &rest[..question];
        }
        parts.path = rest.to_owned();
        parts
    }

    /// `urlsplit` with its validation: a network location with an unbalanced
    /// bracket, text around a bracketed host, or a bracketed host that is
    /// neither an IPv6 address nor an `IPvFuture` literal is refused.
    pub fn try_split(url: &str) -> Result<Self, InvalidUrl> {
        let parts = Self::split(url);
        let netloc = parts.netloc.as_str();
        let opens = netloc.contains('[');
        let closes = netloc.contains(']');
        if opens != closes {
            return Err(InvalidUrl);
        }
        if opens {
            check_bracketed_netloc(netloc)?;
        }
        Ok(parts)
    }

    /// `urlparse`: as `urlsplit`, with the `;params` of the last segment
    /// taken off the path for a scheme that carries them.
    #[must_use]
    pub fn parse(url: &str) -> Self {
        Self::split(url).without_params()
    }

    /// `urlparse` with the validation [`Self::try_split`] applies.
    pub fn try_parse(url: &str) -> Result<Self, InvalidUrl> {
        Self::try_split(url).map(Self::without_params)
    }

    /// `_splitparams`, applied only where `uses_params` names the scheme.
    fn without_params(mut self) -> Self {
        if !USES_PARAMS.contains(&self.scheme.as_str()) {
            return self;
        }
        let start = self.path.rfind('/').unwrap_or(0);
        if let Some(semicolon) = self.path[start..].find(';') {
            self.path.truncate(start + semicolon);
        }
        self
    }

    /// `urlunsplit`.
    #[must_use]
    pub fn unsplit(&self) -> String {
        let mut url = self.path.clone();
        if !self.netloc.is_empty() {
            if !url.is_empty() && !url.starts_with('/') {
                url.insert(0, '/');
            }
            url = format!("//{}{url}", self.netloc);
        } else if url.starts_with("//")
            || (!self.scheme.is_empty()
                && USES_NETLOC.contains(&self.scheme.as_str())
                && (url.is_empty() || url.starts_with('/')))
        {
            url = format!("//{url}");
        }
        if !self.scheme.is_empty() {
            url = format!("{}:{url}", self.scheme);
        }
        if !self.query.is_empty() {
            let _ = write!(url, "?{}", self.query);
        }
        if !self.fragment.is_empty() {
            let _ = write!(url, "#{}", self.fragment);
        }
        url
    }

    /// `SplitResult._hostinfo`: the host and the raw port text, after any
    /// credentials, with a bracketed host taken from between its brackets.
    fn host_info(&self) -> (&str, Option<&str>) {
        let host_info = self
            .netloc
            .rsplit_once('@')
            .map_or(self.netloc.as_str(), |(_, host)| host);
        let (host, port) = match host_info.split_once('[') {
            Some((_, bracketed)) => {
                let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
                (host, after.split_once(':').map_or("", |(_, port)| port))
            }
            None => host_info.split_once(':').unwrap_or((host_info, "")),
        };
        (host, (!port.is_empty()).then_some(port))
    }

    /// `SplitResult.hostname`: lowercased, except an IPv6 zone after `%`.
    #[must_use]
    pub fn hostname(&self) -> Option<String> {
        let (host, _) = self.host_info();
        if host.is_empty() {
            return None;
        }
        Some(match host.split_once('%') {
            Some((address, zone)) => format!("{}%{zone}", address.to_lowercase()),
            None => host.to_lowercase(),
        })
    }

    /// `SplitResult.port`: `None` when omitted, an error when the text is not
    /// ASCII digits or names no port in `0..=65535`.
    pub fn port(&self) -> Result<Option<u16>, InvalidUrl> {
        let (_, port) = self.host_info();
        let Some(port) = port else {
            return Ok(None);
        };
        if !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(InvalidUrl);
        }
        let digits = port.trim_start_matches('0');
        if digits.len() > 5 {
            return Err(InvalidUrl);
        }
        let value: u32 = if digits.is_empty() {
            0
        } else {
            digits.parse().map_err(|_| InvalidUrl)?
        };
        u16::try_from(value).map(Some).map_err(|_| InvalidUrl)
    }
}

/// `_check_bracketed_netloc` and `_check_bracketed_host`.
fn check_bracketed_netloc(netloc: &str) -> Result<(), InvalidUrl> {
    let host_and_port = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    let host = match host_and_port.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err(InvalidUrl);
            }
            let (host, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !after.is_empty() && !after.starts_with(':') {
                return Err(InvalidUrl);
            }
            host
        }
        None => host_and_port
            .split_once(':')
            .map_or(host_and_port, |(host, _)| host),
    };
    if let Some(future) = host.strip_prefix('v') {
        let valid = future.split_once('.').is_some_and(|(version, rest)| {
            !version.is_empty()
                && version.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !rest.is_empty()
                && !rest.contains('\n')
        });
        return if valid { Ok(()) } else { Err(InvalidUrl) };
    }
    let address = host.split_once('%').map_or(host, |(address, _)| address);
    address
        .parse::<Ipv6Addr>()
        .map(|_| ())
        .map_err(|_| InvalidUrl)
}

/// The schemes whose omitted port equals an explicit default one.
pub const DEFAULT_PORTS: [(&str, u16); 2] = [("http", 80), ("https", 443)];

/// The default port of `scheme`, when it has one.
#[must_use]
pub fn default_port(scheme: &str) -> Option<u16> {
    DEFAULT_PORTS
        .iter()
        .find(|(known, _)| *known == scheme)
        .map(|(_, port)| *port)
}

/// An origin as the reference compares it: scheme, host and effective port.
pub type OriginKey = (String, Option<String>, Option<u16>);

/// Reference `normalize_url_origin` (`vibe/setup/auth/browser_sign_in_gateway.py`):
/// the lowercased scheme, the host, and the port with an omitted default one
/// filled in; a malformed port is an error.
pub fn normalize_url_origin(parsed: &PyUrl) -> Result<OriginKey, InvalidUrl> {
    let scheme = parsed.scheme.to_ascii_lowercase();
    let port = parsed.port()?.or_else(|| default_port(&scheme));
    Ok((scheme, parsed.hostname(), port))
}

#[cfg(test)]
mod pyurl_tests;
