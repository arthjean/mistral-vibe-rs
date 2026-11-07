//! The clean-room guard: no tracked file shares an 8-word run of prose with
//! the pinned reference, outside a baseline that may only shrink.
//!
//! `NOTICE` declares that no upstream source is copied into this repository.
//! `python3 scripts/parity/repin.py shingles <sha>` reduces the reference's
//! prose at the pin (comments, docstrings, string literals and Markdown) to
//! the digests of its 8-word shingles, under `crates/vibe-core/tests/licensing/`,
//! and `scripts/parity/shingles.py` states how. This test reads every tracked
//! text file (and every untracked one git does not ignore, which the next
//! commit can hold), except the Apache License text both trees ship, the same
//! way: the escapes `\n`, `\r` and `\t` separate words, a
//! word is a maximal run of ASCII letters and digits, lowercased, and a
//! shingle is eight consecutive words joined by single spaces, hashed to the
//! first eight bytes of its SHA-256. Consecutive shingles found in the set
//! form one shared run of `shingles + 7` words.
//!
//! **The allowlist rule.** A shared run made of code rather than prose says
//! nothing about copied text: identifiers, URLs, protocol method names,
//! configuration keys and numbers are names both sides have to spell alike. A
//! run counts only when it reads as prose. Its source text is split on
//! whitespace after removing URLs (`scheme://...`), each token is stripped of
//! surrounding brackets and punctuation, and a token is a *plain word* when it
//! is made of ASCII letters alone, with at most inner apostrophes or hyphens,
//! is not wrapped in quotes or backticks, and is lowercase, capitalized or
//! all capitals (so not camelCase). Everything else (`snake_case`,
//! `session/turn/start`, `config.key`, `--flag`, `"quoted"`, digits) is code.
//! A run counts when at least half of its tokens, and at least
//! [`PROSE_WORDS`] of them, are plain words.
//!
//! **The baseline.** `crates/vibe-core/tests/licensing/baseline.txt` lists the
//! runs known when the guard landed, each as its file, the digest of its first
//! shingle and its length in words, so it holds no text. A run it does not list
//! fails the test, which is how a new copy is kept out: rewrite the text in
//! original words. A listed run that no longer occurs fails it too, until its
//! line is removed, so the baseline only shrinks. Every run measures the
//! baseline it found and writes it to `target/parity/licensing/baseline.txt`;
//! the test never rewrites the committed one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use sha2::{Digest as _, Sha256};

use super::REFERENCE_COMMIT;
use super::live::records;
use super::replay::repository;

/// How many words a shingle holds.
const WIDTH: usize = 8;

/// The least number of plain words a counted run holds.
const PROSE_WORDS: usize = 4;

/// Where the digest sets and the baseline are committed.
const LICENSING: &str = "crates/vibe-core/tests/licensing";

/// Files that carry a standard text both trees ship verbatim on purpose, which
/// the reference did not author: the Apache License 2.0.
const STANDARD_TEXTS: [&str; 1] = ["LICENSE"];

/// One word of a scanned file: where it starts and ends, and its line.
struct Word {
    start: usize,
    end: usize,
    line: usize,
}

/// One shared run of a tracked file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Run {
    file: String,
    line: usize,
    /// The digest of its first shingle, which identifies it without its text.
    first: u64,
    /// Its length in words.
    words: usize,
}

impl Run {
    fn key(&self) -> (String, u64, usize) {
        (self.file.clone(), self.first, self.words)
    }
}

/// The words of `text` under the normalization `scripts/parity/shingles.py`
/// states: an escape `\n`, `\r` or `\t` separates, a word is a run of ASCII
/// letters and digits.
fn words(text: &str) -> Vec<Word> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut line = 1;
    let mut start: Option<(usize, usize)> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        let escaped = matches!(byte, b'n' | b'r' | b't') && index > 0 && bytes[index - 1] == b'\\';
        let word = byte.is_ascii_alphanumeric() && !escaped;
        match (word, start) {
            (true, None) => start = Some((index, line)),
            (false, Some((begin, at))) => {
                found.push(Word {
                    start: begin,
                    end: index,
                    line: at,
                });
                start = None;
            }
            _ => {}
        }
        if byte == b'\n' {
            line += 1;
        }
    }
    if let Some((begin, at)) = start {
        found.push(Word {
            start: begin,
            end: bytes.len(),
            line: at,
        });
    }
    found
}

/// The digest of the shingle the eight `words` of `text` form.
fn digest(text: &str, words: &[Word], buffer: &mut String) -> u64 {
    buffer.clear();
    for (index, word) in words.iter().enumerate() {
        if index > 0 {
            buffer.push(' ');
        }
        buffer.extend(
            text[word.start..word.end]
                .chars()
                .map(|c| c.to_ascii_lowercase()),
        );
    }
    let hash = Sha256::digest(buffer.as_bytes());
    let mut first = [0; 8];
    first.copy_from_slice(&hash[..8]);
    u64::from_be_bytes(first)
}

/// Whether `token`, stripped of surrounding brackets and punctuation, is a
/// plain word of prose.
fn plain(token: &str) -> bool {
    let token = token
        .trim_start_matches(['(', '[', '{', '<', '*', '_'])
        .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '*', '_']);
    if token.is_empty()
        || token.starts_with(['"', '\'', '`'])
        || token.ends_with(['"', '\'', '`'])
        || !token
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '-')
        || !token.starts_with(|c: char| c.is_ascii_alphabetic())
        || !token.ends_with(|c: char| c.is_ascii_alphabetic())
        || token.contains("--")
        || token.contains("''")
    {
        return false;
    }
    let letters: Vec<char> = token.chars().filter(char::is_ascii_alphabetic).collect();
    let rest_lower = letters[1..].iter().all(char::is_ascii_lowercase);
    let all_upper = letters.iter().all(char::is_ascii_uppercase);
    rest_lower || all_upper
}

/// Whether the source text of a run reads as prose, under the allowlist rule
/// this module states.
fn is_prose(span: &str) -> bool {
    let tokens: Vec<&str> = span
        .split_whitespace()
        .filter(|token| !token.contains("://"))
        .collect();
    let plain = tokens.iter().filter(|token| plain(token)).count();
    plain >= PROSE_WORDS && plain * 2 >= tokens.len()
}

/// Every shared run of `text`, the tracked file `file`, that reads as prose.
fn runs(file: &str, text: &str, set: &[u64]) -> Vec<Run> {
    let words = words(text);
    if words.len() < WIDTH {
        return Vec::new();
    }
    let mut buffer = String::new();
    let matched: Vec<Option<u64>> = (0..=words.len() - WIDTH)
        .map(|start| {
            let value = digest(text, &words[start..start + WIDTH], &mut buffer);
            set.binary_search(&value).ok().map(|_| value)
        })
        .collect();
    let mut found = Vec::new();
    let mut index = 0;
    while index < matched.len() {
        let Some(first) = matched[index] else {
            index += 1;
            continue;
        };
        let begin = index;
        while index < matched.len() && matched[index].is_some() {
            index += 1;
        }
        let last = index - 1 + WIDTH - 1;
        let span = &text[words[begin].start..words[last].end];
        if is_prose(span) {
            found.push(Run {
                file: file.to_owned(),
                line: words[begin].line,
                first,
                words: index - begin + WIDTH - 1,
            });
        }
    }
    found
}

/// The digest set committed for `commit`.
fn digest_set(root: &Path, commit: &str) -> Vec<u64> {
    let short = &commit[..12];
    let path = root.join(LICENSING).join(format!("shingles-{short}.bin"));
    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{} is unreadable ({error}); generate it with `python3 scripts/parity/repin.py \
             shingles {commit}`",
            path.display()
        )
    });
    assert!(
        !bytes.is_empty() && bytes.len().is_multiple_of(8) && bytes.len() <= 4 * 1024 * 1024,
        "{} is not a set of 64-bit digests of at most 4 MB; regenerate it with `python3 \
         scripts/parity/repin.py shingles {commit}`",
        path.display()
    );
    let set: Vec<u64> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| u64::from_be_bytes(*chunk))
        .collect();
    assert!(
        set.windows(2).all(|pair| pair[0] < pair[1]),
        "{} is not sorted and distinct; regenerate it",
        path.display()
    );
    set
}

/// Every file git tracks, and every untracked file it does not ignore (what
/// the next commit can hold), workspace-relative.
fn tracked(root: &Path) -> Vec<String> {
    let listing = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .expect("git runs, which the guard needs to know what is tracked");
    assert!(
        listing.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&listing.stderr)
    );
    String::from_utf8(listing.stdout)
        .expect("tracked paths are UTF-8")
        .split('\0')
        .filter(|name| {
            !name.is_empty() && !name.starts_with(LICENSING) && !STANDARD_TEXTS.contains(name)
        })
        .map(str::to_owned)
        .collect()
}

/// Every shared run of every tracked text file, scanned on every core.
fn scan(root: &Path, files: &[String], set: &[u64]) -> Vec<Run> {
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let chunk = files.len().div_ceil(workers).max(1);
    let mut found: Vec<Run> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .filter_map(|file| {
                            let bytes = std::fs::read(root.join(file)).ok()?;
                            let text = String::from_utf8(bytes).ok()?;
                            (!text.contains('\0')).then(|| runs(file, &text, set))
                        })
                        .flatten()
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("a scan thread finishes"))
            .collect()
    });
    found.sort();
    found
}

/// The committed baseline, as a count per run.
fn baseline(root: &Path) -> BTreeMap<(String, u64, usize), usize> {
    let path = root.join(LICENSING).join("baseline.txt");
    let text = std::fs::read_to_string(&path).expect("the licensing baseline is committed");
    let mut entries = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let parsed = (|| {
            let file = fields.next()?.to_owned();
            let first = u64::from_str_radix(fields.next()?, 16).ok()?;
            let words = fields.next()?.parse().ok()?;
            fields.next().is_none().then_some((file, first, words))
        })();
        let key = parsed.unwrap_or_else(|| {
            panic!(
                "{}:{} is not `<file>\\t<first shingle>\\t<words>`",
                path.display(),
                number + 1
            )
        });
        *entries.entry(key).or_default() += 1;
    }
    entries
}

/// The baseline `runs` would need, in the committed format.
fn render(runs: &[Run]) -> String {
    let mut text = String::from(
        "# Runs of 8 or more words this repository shares with the pinned reference's prose,\n\
         # known when the clean-room guard landed (crates/vibe-core/src/parity/licensing_tests.rs).\n\
         # Each line is a file, the digest of the run's first shingle and its length in words.\n\
         # Lines may only be removed: rewrite a run in original words, then delete its line.\n",
    );
    for run in runs {
        let _ = writeln!(text, "{}\t{:016x}\t{}", run.file, run.first, run.words);
    }
    text
}

#[test]
fn no_tracked_file_shares_prose_with_the_reference_outside_the_baseline() {
    let started = Instant::now();
    let root = repository();
    let set = digest_set(&root, REFERENCE_COMMIT);
    let files = tracked(&root);
    let found = scan(&root, &files, &set);

    let measured = records().join("licensing");
    if std::fs::create_dir_all(&measured).is_ok() {
        let _ = std::fs::write(measured.join("baseline.txt"), render(&found));
    }

    let mut remaining = baseline(&root);
    let mut unlisted = Vec::new();
    for run in &found {
        match remaining.get_mut(&run.key()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => unlisted.push(run),
        }
    }
    let stale: Vec<String> = remaining
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|((file, first, words), _)| format!("{file}\t{first:016x}\t{words}"))
        .collect();

    println!(
        "licensing guard: {} tracked files, {} reference shingles, {} shared runs, {} unlisted, \
         {} stale, in {:.1?}",
        files.len(),
        set.len(),
        found.len(),
        unlisted.len(),
        stale.len(),
        started.elapsed()
    );
    assert!(
        unlisted.is_empty(),
        "these tracked files share runs of prose with the reference at {} that \
         {LICENSING}/baseline.txt does not list; rewrite them in original words (NOTICE), and \
         ledger a user-visible message under Accepted divergences in docs/parity.md:\n{}",
        &REFERENCE_COMMIT[..12],
        unlisted
            .iter()
            .map(|run| format!("{}:{} shares a {}-word run", run.file, run.line, run.words))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        stale.is_empty(),
        "these lines of {LICENSING}/baseline.txt no longer occur; remove them, since the \
         baseline only shrinks (the measured baseline is in {}):\n{}",
        measured.join("baseline.txt").display(),
        stale.join("\n")
    );
}

#[test]
fn the_normalization_matches_the_one_the_digest_set_was_built_with() {
    // `scripts/parity/shingles.py` builds the set; these are the digests its
    // `digest(words(...))` answers for the same text.
    let text = "Hello, World!\\nThe quick brown fox jumps over the lazy dog";
    let found = words(text);
    let normalized: Vec<String> = found
        .iter()
        .map(|word| text[word.start..word.end].to_ascii_lowercase())
        .collect();
    assert_eq!(
        normalized,
        [
            "hello", "world", "the", "quick", "brown", "fox", "jumps", "over", "the", "lazy", "dog"
        ]
    );
    let mut buffer = String::new();
    let first = digest(text, &found[..WIDTH], &mut buffer);
    assert_eq!(buffer, "hello world the quick brown fox jumps over");
    assert_eq!(first, 0xe6da_98e7_5de5_ba6d_u64, "{first:016x}");
}

#[test]
fn a_run_of_identifiers_is_not_prose() {
    assert!(!is_prose(
        "`session/turn/start` `session/turn/cancel` `max_tokens` https://example.com/a b c"
    ));
    assert!(!is_prose(
        r#""temperature", "model", "top_p", "max_tokens", "stop""#
    ));
    assert!(is_prose(
        "Set the limit before the model answers, or the reply is cut"
    ));
    assert!(is_prose(
        "Returns the `session_id` of the session the turn belongs to"
    ));
}

#[test]
fn consecutive_shared_shingles_form_one_run() {
    let text = "one two three four five six seven eight nine ten";
    let all = words(text);
    let mut buffer = String::new();
    let mut set: Vec<u64> = (0..=all.len() - WIDTH)
        .map(|start| digest(text, &all[start..start + WIDTH], &mut buffer))
        .collect();
    set.sort_unstable();
    // Digits-free plain words, so the run counts as prose.
    let found = runs("sample.md", text, &set);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].words, 10);
    assert_eq!(found[0].line, 1);
}
