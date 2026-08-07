//! Decoding a file the way the reference decodes one, and writing it back the
//! way it found it.
//!
//! Reference `decode_safe` (`vibe/utils/io.py:103`) tries the byte-order mark,
//! then UTF-8, then the locale codec, then `charset-normalizer`, and normalizes
//! every line ending to `\n` while remembering which one the file used. `edit`
//! then writes the file back through `atomic_replace` with that codec and that
//! line ending, so a one-line change to a CRLF file written in a single-byte
//! codec does not rewrite the whole file.
//!
//! This port covers the same round trip over a narrower codec set: the marks it
//! can recognize, UTF-8, and Latin-1 as the fallback that always decodes. A
//! statistical detector is a dependency this repository has no other use for,
//! and the codecs below are the ones that survive the round trip byte for byte.
//! The bound is recorded in `docs/parity.md`.

/// The codec a file was decoded with, which is the one it is written back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8, the codec this repository's own files use.
    Utf8,
    /// UTF-8 behind a byte-order mark, which is restored on write.
    Utf8Bom,
    /// The single-byte fallback: every byte decodes, so no file is refused.
    Latin1,
}

/// The line ending a file used, which is the one it is written back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newline {
    Lf,
    CrLf,
    Cr,
}

impl Newline {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

/// A file's text, normalized to `\n`, plus what it takes to write it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedText {
    pub text: String,
    pub encoding: Encoding,
    pub newline: Newline,
}

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Decodes `bytes`, normalizing the line endings and recording both.
#[must_use]
pub fn decode(bytes: &[u8]) -> DecodedText {
    let (encoding, body) = match bytes.strip_prefix(UTF8_BOM) {
        Some(rest) => (Encoding::Utf8Bom, rest),
        None => (Encoding::Utf8, bytes),
    };
    let (encoding, raw) = match std::str::from_utf8(body) {
        Ok(text) => (encoding, text.to_owned()),
        // Latin-1 maps every byte onto a code point, so this branch cannot
        // fail: a file is never refused for its encoding.
        Err(_) => (
            Encoding::Latin1,
            body.iter().map(|byte| char::from(*byte)).collect(),
        ),
    };
    let (text, newline) = normalize_newlines(&raw);
    DecodedText {
        text,
        encoding,
        newline,
    }
}

/// The inverse of [`decode`]: `\n` back to the original ending, then the codec.
#[must_use]
pub fn encode(text: &str, encoding: Encoding, newline: Newline) -> Vec<u8> {
    let body = if newline == Newline::Lf {
        text.to_owned()
    } else {
        text.replace('\n', newline.as_str())
    };
    match encoding {
        Encoding::Utf8 => body.into_bytes(),
        Encoding::Utf8Bom => {
            let mut bytes = UTF8_BOM.to_vec();
            bytes.extend_from_slice(body.as_bytes());
            bytes
        }
        // A character the codec cannot hold falls back to its UTF-8 bytes, so
        // an edit that introduces one is written rather than refused.
        Encoding::Latin1 => body
            .chars()
            .flat_map(|character| {
                u8::try_from(u32::from(character))
                    .map_or_else(|_| character.to_string().into_bytes(), |byte| vec![byte])
            })
            .collect(),
    }
}

/// The text with `\n` endings and the ending it had, counting the way the
/// reference counts: the most frequent style wins, and a file with none keeps
/// the platform's.
#[must_use]
pub fn normalize_newlines(text: &str) -> (String, Newline) {
    if !text.contains('\r') {
        return (text.to_owned(), Newline::Lf);
    }
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count().saturating_sub(crlf);
    let cr = text.matches('\r').count().saturating_sub(crlf);
    let newline = if crlf >= lf && crlf >= cr {
        Newline::CrLf
    } else if lf >= cr {
        Newline::Lf
    } else {
        Newline::Cr
    };
    (text.replace("\r\n", "\n").replace('\r', "\n"), newline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crlf_file_round_trips_through_its_own_line_ending() {
        let raw = b"first line\r\nsecond line\r\n";
        let decoded = decode(raw);
        assert_eq!(decoded.text, "first line\nsecond line\n");
        assert_eq!(decoded.newline, Newline::CrLf);
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(
            encode(&decoded.text, decoded.encoding, decoded.newline),
            raw
        );
    }

    #[test]
    fn a_latin1_file_round_trips_byte_for_byte() {
        let raw = b"caf\xe9\r\n";
        let decoded = decode(raw);
        assert_eq!(decoded.text, "café\n");
        assert_eq!(decoded.encoding, Encoding::Latin1);
        assert_eq!(decoded.newline, Newline::CrLf);
        assert_eq!(
            encode(&decoded.text, decoded.encoding, decoded.newline),
            raw
        );
    }

    #[test]
    fn a_byte_order_mark_is_restored_rather_than_decoded_into_the_text() {
        let mut raw = UTF8_BOM.to_vec();
        raw.extend_from_slice(b"body\n");
        let decoded = decode(&raw);
        assert_eq!(decoded.text, "body\n");
        assert_eq!(decoded.encoding, Encoding::Utf8Bom);
        assert_eq!(
            encode(&decoded.text, decoded.encoding, decoded.newline),
            raw
        );
    }

    #[test]
    fn a_file_with_no_line_ending_keeps_the_default() {
        let decoded = decode(b"single");
        assert_eq!(decoded.newline, Newline::Lf);
        assert_eq!(decoded.text, "single");
    }

    #[test]
    fn the_dominant_ending_wins_a_mixed_file() {
        let (_, newline) = normalize_newlines("a\r\nb\r\nc\n");
        assert_eq!(newline, Newline::CrLf);
        let (_, newline) = normalize_newlines("a\rb\rc\r\nd\n");
        assert_eq!(newline, Newline::Cr);
    }
}
