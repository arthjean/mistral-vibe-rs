//! Reference `packaging.version.Version`, the PEP 440 parser the update notifier
//! compares every version with.
//!
//! The grammar, the normalization `str(Version)` produces, and the ordering its
//! `_cmpkey` defines are reproduced whole, because the notifier feeds it whatever
//! a release index, a tag, or a cache file holds. Numbers are kept as decimal
//! digits rather than machine integers: Python's `int` has no ceiling, so a
//! segment wider than 64 bits is still a version upstream.

use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;

/// Reference `VERSION_PATTERN`, anchored as `Version._regex.fullmatch` anchors
/// it. The reference restricts the version itself to ASCII with `(?a:`, which is
/// `(?-u:` here; the whitespace around it is stripped separately because it is
/// the one part the reference matches with Unicode semantics.
static VERSION_PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?x)(?i-u)
        ^v?
        (?:(?P<epoch>[0-9]+)!)?
        (?P<release>[0-9]+(?:\.[0-9]+)*)
        (?P<pre>
            [._-]?
            (?P<pre_l>alpha|a|beta|b|preview|pre|c|rc)
            [._-]?
            (?P<pre_n>[0-9]+)?
        )?
        (?P<post>
            (?:-(?P<post_n1>[0-9]+))
            |
            (?:[._-]?(?P<post_l>post|rev|r)[._-]?(?P<post_n2>[0-9]+)?)
        )?
        (?P<dev>
            [._-]?
            (?P<dev_l>dev)
            [._-]?
            (?P<dev_n>[0-9]+)?
        )?
        (?:\+(?P<local>[a-z0-9]+(?:[._-][a-z0-9]+)*))?
        $",
    )
    .ok()
});

/// A non-negative integer of any width, held as its decimal digits without
/// leading zeros, which is what `int()` makes of a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Number(String);

impl Number {
    fn parse(digits: &str) -> Self {
        let trimmed = digits.trim_start_matches('0');
        Self(if trimmed.is_empty() {
            "0".to_owned()
        } else {
            trimmed.to_owned()
        })
    }

    fn zero() -> Self {
        Self("0".to_owned())
    }

    fn is_zero(&self) -> bool {
        self.0 == "0"
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Reference `_PRE_RANK`, whose order is the order of the normalized letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PreKind {
    Alpha,
    Beta,
    ReleaseCandidate,
}

impl PreKind {
    /// Reference `_LETTER_NORMALIZATION` applied to a lowercased letter.
    fn from_letter(letter: &str) -> Option<Self> {
        match letter {
            "a" | "alpha" => Some(Self::Alpha),
            "b" | "beta" => Some(Self::Beta),
            "rc" | "c" | "pre" | "preview" => Some(Self::ReleaseCandidate),
            _ => None,
        }
    }

    const fn letter(self) -> &'static str {
        match self {
            Self::Alpha => "a",
            Self::Beta => "b",
            Self::ReleaseCandidate => "rc",
        }
    }
}

/// One segment of a local version: `_parse_local_version` keeps an all-digit
/// part as an integer and lowercases every other one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalSegment {
    Text(String),
    Number(Number),
}

impl Ord for LocalSegment {
    /// Reference `_cmpkey`: `(-1, text)` against `(n, "")`, so any text sorts
    /// before any number and text compares lexicographically.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => left.cmp(right),
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Text(_), Self::Number(_)) => Ordering::Less,
            (Self::Number(_), Self::Text(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for LocalSegment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for LocalSegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter.write_str(text),
            Self::Number(number) => number.fmt(formatter),
        }
    }
}

/// Reference `packaging.version.Version`.
///
/// Equality and ordering are both the reference `_cmpkey`, so `1.0` and
/// `1.0.0` are equal while each still prints the release it was written with.
#[derive(Debug, Clone)]
pub struct Version {
    epoch: Number,
    release: Vec<Number>,
    pre: Option<(PreKind, Number)>,
    post: Option<Number>,
    dev: Option<Number>,
    local: Option<Vec<LocalSegment>>,
}

impl Version {
    /// Reference `Version(raw)`: `None` wherever the reference raises
    /// `InvalidVersion`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let captures = VERSION_PATTERN
            .as_ref()?
            .captures(raw.trim_matches(is_python_space))?;
        let number = |name: &str| {
            captures
                .name(name)
                .map(|found| Number::parse(found.as_str()))
        };
        let release = captures
            .name("release")?
            .as_str()
            .split('.')
            .map(Number::parse)
            .collect();
        let pre = match captures.name("pre_l") {
            Some(letter) => Some((
                PreKind::from_letter(&letter.as_str().to_ascii_lowercase())?,
                number("pre_n").unwrap_or_else(Number::zero),
            )),
            None => None,
        };
        let post = match (captures.name("post_n1"), captures.name("post_l")) {
            (Some(implicit), _) => Some(Number::parse(implicit.as_str())),
            (None, Some(_)) => Some(number("post_n2").unwrap_or_else(Number::zero)),
            (None, None) => None,
        };
        let dev = captures
            .name("dev_l")
            .map(|_| number("dev_n").unwrap_or_else(Number::zero));
        let local = captures.name("local").map(|local| {
            local
                .as_str()
                .split(['.', '_', '-'])
                .map(|part| {
                    if part.bytes().all(|byte| byte.is_ascii_digit()) {
                        LocalSegment::Number(Number::parse(part))
                    } else {
                        LocalSegment::Text(part.to_ascii_lowercase())
                    }
                })
                .collect()
        });
        Some(Self {
            epoch: number("epoch").unwrap_or_else(Number::zero),
            release,
            pre,
            post,
            dev,
            local,
        })
    }

    /// Reference `_parse_version` in `vibe/cli/update_notifier/update.py`: every
    /// dash becomes a local-version separator before the version is parsed, so
    /// a build suffix such as `2.24.0-dev` compares as a local version.
    #[must_use]
    pub fn parse_notifier(raw: &str) -> Option<Self> {
        Self::parse(&raw.replace('-', "+"))
    }

    /// The release with its trailing zeros removed, which is what `_cmpkey`
    /// compares.
    fn trimmed_release(&self) -> &[Number] {
        let kept = self
            .release
            .iter()
            .rposition(|segment| !segment.is_zero())
            .map_or(0, |index| index + 1);
        &self.release[..kept]
    }

    /// Reference `_cmpkey`'s suffix, `(pre_rank, pre_n, post_rank, post_n,
    /// dev_rank, dev_n)`, with the ranks as small integers.
    fn suffix(&self) -> (i8, Number, u8, Number, u8, Number) {
        let (pre_rank, pre_number) = match (&self.pre, &self.post, &self.dev) {
            (None, None, Some(_)) => (-1, Number::zero()),
            (None, _, _) => (3, Number::zero()),
            (Some((kind, number)), _, _) => (*kind as i8, number.clone()),
        };
        let (post_rank, post_number) = self
            .post
            .as_ref()
            .map_or((0, Number::zero()), |post| (1, post.clone()));
        let (dev_rank, dev_number) = self
            .dev
            .as_ref()
            .map_or((1, Number::zero()), |dev| (0, dev.clone()));
        (
            pre_rank,
            pre_number,
            post_rank,
            post_number,
            dev_rank,
            dev_number,
        )
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.trimmed_release().cmp(other.trimmed_release()))
            .then_with(|| self.suffix().cmp(&other.suffix()))
            // A version without a local segment is the shorter key tuple, so
            // it sorts first.
            .then_with(|| self.local.cmp(&other.local))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    /// Reference `Version.__str__`, the normalized spelling.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.epoch.is_zero() {
            write!(formatter, "{}!", self.epoch)?;
        }
        let release = self
            .release
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        formatter.write_str(&release.join("."))?;
        if let Some((kind, number)) = &self.pre {
            write!(formatter, "{}{number}", kind.letter())?;
        }
        if let Some(post) = &self.post {
            write!(formatter, ".post{post}")?;
        }
        if let Some(dev) = &self.dev {
            write!(formatter, ".dev{dev}")?;
        }
        if let Some(local) = &self.local {
            let local = local.iter().map(ToString::to_string).collect::<Vec<_>>();
            write!(formatter, "+{}", local.join("."))?;
        }
        Ok(())
    }
}

/// `str.isspace()`, which is what the reference's `\s` matches in a text
/// pattern: Unicode whitespace plus the four ASCII separators `U+001C` through
/// `U+001F`, which the Unicode `White_Space` property leaves out.
pub(super) fn is_python_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// Reference `_parse_filename_version` in
/// `vibe/cli/update_notifier/adapters/pypi_update_gateway.py`: the version of a
/// wheel, else of a source distribution, else nothing, with each filename held
/// to the rules `parse_wheel_filename` and `parse_sdist_filename` apply.
#[must_use]
pub fn artifact_version(filename: &str) -> Option<Version> {
    wheel_version(filename).or_else(|| sdist_version(filename))
}

/// Reference `packaging.utils.parse_wheel_filename`, reduced to the version.
fn wheel_version(filename: &str) -> Option<Version> {
    let stem = filename.strip_suffix(".whl")?;
    let dashes = stem.matches('-').count();
    if dashes != 4 && dashes != 5 {
        return None;
    }
    let parts = stem.splitn(dashes - 1, '-').collect::<Vec<_>>();
    let name = *parts.first()?;
    // `re.match(r"^[\w\d._]*$")`: `$` also matches before one final newline.
    let name_body = name.strip_suffix('\n').unwrap_or(name);
    if name.contains("__")
        || !name_body
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
    {
        return None;
    }
    let version = Version::parse(parts.get(1)?)?;
    if dashes == 5 && !parts.get(2)?.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(version)
}

/// Reference `packaging.utils.parse_sdist_filename`, reduced to the version.
fn sdist_version(filename: &str) -> Option<Version> {
    let stem = filename
        .strip_suffix(".tar.gz")
        .or_else(|| filename.strip_suffix(".zip"))?;
    let (_, version) = stem.rsplit_once('-')?;
    Version::parse(version)
}

#[cfg(test)]
mod version_tests;
