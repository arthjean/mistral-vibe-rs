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
//! **The third-party layer.** The reference's tree also quotes text it did
//! not author, such as a library's error message in a test that asserts it,
//! or a license header. `repin.py shingles` writes beside the digest set the
//! digests a locked third-party distribution of the attested environment
//! also ships (`third-party-<sha12>.json`, with the distributions that ship
//! each, the lockfile digest and the platform it was measured on), and the
//! guard subtracts them, so a run that reproduces `pydantic`'s or `httpx2`'s
//! own words is not the reference's to claim. The layer holds no text, is
//! refused when it names another commit or a digest the set does not hold,
//! and `repin.py shingles --check` recomputes it on the weekly live run. The
//! digest set of `PREVIOUS_COMMIT` is read as committed, since the baseline
//! was cut against it.
//!
//! **The baseline.** `crates/vibe-core/tests/licensing/baseline.txt` lists the
//! runs known when the guard landed, each as its file, the digest of its first
//! shingle and its length in words, so it holds no text. A run it does not list
//! fails the test, which is how a new copy is kept out: rewrite the text in
//! original words. A listed run that no longer occurs fails it too, until its
//! line is removed, so the baseline only shrinks. Every run measures the
//! baseline it found and writes it to `target/parity/licensing/baseline.txt`;
//! the test never rewrites the committed one.
//!
//! **During a rolling re-pin** the baseline stays cut at `PREVIOUS_COMMIT`,
//! whose digest set stays committed: a listed run is stale when it no longer
//! occurs there, and a run the target finds is admitted when the baseline
//! lists it (as often as the line counts) or when it re-cuts a listed run,
//! covering some of the same words of the same file and fewer than a
//! shingle's worth outside them (a new pin moves the boundaries of a copy the
//! baseline already names). A run over text no listed run covers fails, as it
//! does outside a transition. The change that clears `PREVIOUS_COMMIT` also
//! re-cuts the baseline at the target, from the measured one, after the guard
//! passed with the previous pin still named: every line it gains re-cuts a
//! line it drops, so the re-cut never grows the baseline, and the previous
//! digest set leaves with it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use sha2::{Digest as _, Sha256};

use super::live::records;
use super::replay::repository;
use super::{PREVIOUS_COMMIT, REFERENCE_COMMIT};

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
    /// The index of its first word in the file, which tells whether two cuts
    /// of the same text overlap.
    start: usize,
}

impl Run {
    fn key(&self) -> (String, u64, usize) {
        (self.file.clone(), self.first, self.words)
    }

    /// Whether `other` covers some of the same words of the same file.
    fn overlaps(&self, other: &Self) -> bool {
        self.file == other.file
            && self.start < other.start + other.words
            && other.start < self.start + self.words
    }
}

/// The runs of `found` the baseline does not admit, and its stale lines.
///
/// Outside a transition, `found` is measured at the target and `prior` is
/// empty: a run is admitted when the baseline lists it, and a listed run that
/// no longer occurs is stale. During a rolling re-pin the baseline stays cut
/// at the previous commit, whose runs are `prior`: staleness is measured
/// there, and a run at the target is also admitted when it re-cuts a listed
/// run of `prior` ([`recuts`]), since a new pin moves the boundaries of a copy
/// the baseline already names. A run the target reveals over text no listed
/// run covers is not admitted, and a listed line admits at most as many runs
/// as it counts.
fn judge<'r>(
    found: &'r [Run],
    prior: Option<&[Run]>,
    listed: &BTreeMap<(String, u64, usize), usize>,
) -> (Vec<&'r Run>, Vec<String>) {
    let mut remaining = listed.clone();
    let mut admitted_prior = Vec::new();
    for run in prior.unwrap_or(found) {
        if let Some(count) = remaining.get_mut(&run.key())
            && *count > 0
        {
            *count -= 1;
            admitted_prior.push(run);
        }
    }
    let stale = remaining
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|((file, first, words), _)| format!("{file}\t{first:016x}\t{words}"))
        .collect();
    // A listed line admits as many runs as it counts, at either commit, so a
    // second paste of a listed copy is not admitted by the first one's line.
    let mut budget = listed.clone();
    let unlisted = found
        .iter()
        .filter(|run| {
            if let Some(count) = budget.get_mut(&run.key())
                && *count > 0
            {
                *count -= 1;
                return false;
            }
            prior.is_none() || !recuts(run, &admitted_prior)
        })
        .collect();
    (unlisted, stale)
}

/// Whether `run`, found at the target, re-cuts the listed runs `prior` found
/// at the previous commit: it overlaps at least one of them, and fewer than a
/// shingle's worth of its words lie outside every one, so the new cut reveals
/// no copy the baseline does not already name. A run that grows past a listed
/// one by a shingle or more holds text no listed run covers.
fn recuts(run: &Run, prior: &[&Run]) -> bool {
    let mut spans: Vec<(usize, usize)> = prior
        .iter()
        .filter(|known| known.overlaps(run))
        .map(|known| (known.start, known.start + known.words))
        .collect();
    if spans.is_empty() {
        return false;
    }
    spans.sort_unstable();
    let end = run.start + run.words;
    let mut cursor = run.start;
    let mut covered = 0;
    for (begin, finish) in spans {
        let (begin, finish) = (begin.max(cursor), finish.min(end));
        if finish > begin {
            covered += finish - begin;
            cursor = finish;
        }
    }
    run.words.saturating_sub(covered) < WIDTH
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
                start: begin,
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

/// The digests of `reference` that the third-party layer of `commit` says a
/// locked third-party distribution also ships, sorted.
///
/// The layer is generated with the digest set and holds no text: the
/// lockfile digest and the environment it was measured on, the distributions
/// it read, and for each digest the distributions that ship it. A layer that
/// names another commit, attributes a digest to no distribution it read, or
/// subtracts a digest the set does not hold was generated against something
/// else, and the guard refuses it rather than subtract it.
fn third_party_layer(root: &Path, commit: &str, reference: &[u64]) -> Vec<u64> {
    let short = &commit[..12];
    let path = root
        .join(LICENSING)
        .join(format!("third-party-{short}.json"));
    let regenerate =
        format!("regenerate it with `python3 scripts/parity/repin.py shingles {commit}`");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is unreadable ({error}); {regenerate}", path.display()));
    let layer: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON ({error}); {regenerate}", path.display()));
    let problems = layer_problems(&layer, commit, reference);
    assert!(
        problems.is_empty(),
        "{} is not the third-party layer of {short}; {regenerate}:\n{}",
        path.display(),
        problems.join("\n")
    );
    third_party_digests(&layer)
}

/// The digests a third-party layer subtracts, sorted.
fn third_party_digests(layer: &serde_json::Value) -> Vec<u64> {
    let mut values: Vec<u64> = layer["digests"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(digest, _)| u64::from_str_radix(digest, 16).ok())
        .collect();
    values.sort_unstable();
    values
}

/// What makes `layer` something other than the third-party layer of `commit`
/// over the digest set `reference`.
fn layer_problems(layer: &serde_json::Value, commit: &str, reference: &[u64]) -> Vec<String> {
    let mut problems = Vec::new();
    if layer["schemaVersion"] != 1 {
        problems.push("its schemaVersion is not 1".to_owned());
    }
    if layer["commit"] != commit {
        problems.push(format!("it names the commit {}", layer["commit"]));
    }
    let lock = layer["uvLockSha256"].as_str().unwrap_or_default();
    if lock.len() != 64 || !lock.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        problems.push("it names no uv.lock digest".to_owned());
    }
    if layer["measuredOn"]["platform"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        problems.push("it names no platform it was measured on".to_owned());
    }
    let read: Vec<&str> = layer["distributions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let Some(digests) = layer["digests"].as_object() else {
        problems.push("it holds no digests object".to_owned());
        return problems;
    };
    for (digest, labels) in digests {
        let value = (digest.len() == 16)
            .then(|| u64::from_str_radix(digest, 16).ok())
            .flatten();
        let Some(value) = value else {
            problems.push(format!("`{digest}` is not a 64-bit digest in hexadecimal"));
            continue;
        };
        if reference.binary_search(&value).is_err() {
            problems.push(format!("{digest} is not a digest of the reference's prose"));
        }
        let labels: Vec<&str> = labels
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if labels.is_empty() || labels.iter().any(|label| !read.contains(label)) {
            problems.push(format!(
                "{digest} is attributed to {labels:?}, not to distributions the layer read"
            ));
        }
    }
    problems
}

/// `reference` without the digests of `layer`, both sorted.
fn subtract(reference: &[u64], layer: &[u64]) -> Vec<u64> {
    reference
        .iter()
        .copied()
        .filter(|value| layer.binary_search(value).is_err())
        .collect()
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
    let reference = digest_set(&root, REFERENCE_COMMIT);
    let layer = third_party_layer(&root, REFERENCE_COMMIT, &reference);
    let set = subtract(&reference, &layer);
    let files = tracked(&root);
    let found = scan(&root, &files, &set);

    let measured = records().join("licensing");
    if std::fs::create_dir_all(&measured).is_ok() {
        let _ = std::fs::write(measured.join("baseline.txt"), render(&found));
    }

    let prior = (!PREVIOUS_COMMIT.is_empty())
        .then(|| scan(&root, &files, &digest_set(&root, PREVIOUS_COMMIT)));
    let listed = baseline(&root);
    let (unlisted, stale) = judge(&found, prior.as_deref(), &listed);

    println!(
        "licensing guard: {} tracked files, {} reference shingles less {} a locked third-party \
         distribution ships, {} shared runs, {} unlisted, {} stale, in {:.1?}",
        files.len(),
        reference.len(),
        layer.len(),
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

fn run(file: &str, first: u64, words: usize, start: usize) -> Run {
    Run {
        file: file.to_owned(),
        line: 1,
        first,
        words,
        start,
    }
}

#[test]
fn a_transition_admits_a_recut_of_a_listed_run_and_nothing_else() {
    let listed = BTreeMap::from([(("a.rs".to_owned(), 1, 10), 1)]);
    let prior = [run("a.rs", 1, 10, 100)];
    // The target cuts the same copy from a later word, and finds a run over
    // text no listed run covers, in the same file and in another.
    let recut = run("a.rs", 7, 12, 104);
    let fresh = run("a.rs", 8, 9, 300);
    let elsewhere = run("b.rs", 9, 12, 104);
    let found = [recut.clone(), fresh.clone(), elsewhere.clone()];
    let (unlisted, stale) = judge(&found, Some(&prior), &listed);
    assert_eq!(unlisted, [&fresh, &elsewhere]);
    assert!(
        stale.is_empty(),
        "staleness is measured at the previous commit"
    );

    let (_, stale) = judge(&found, Some(&[]), &listed);
    assert_eq!(stale, ["a.rs\t0000000000000001\t10"]);
}

#[test]
fn a_transition_refuses_a_run_that_grows_past_a_listed_run_by_a_shingle() {
    let listed = BTreeMap::from([(("a.rs".to_owned(), 1, 10), 1)]);
    let prior = [run("a.rs", 1, 10, 100)];
    // Seven words past the listed copy is a new cut of it; eight words past
    // it is text the baseline does not name, pasted beside a listed copy.
    let recut = run("a.rs", 7, 17, 100);
    let grown = run("a.rs", 8, 18, 100);
    let (unlisted, _) = judge(std::slice::from_ref(&recut), Some(&prior), &listed);
    assert!(unlisted.is_empty(), "{unlisted:?}");
    let (unlisted, _) = judge(std::slice::from_ref(&grown), Some(&prior), &listed);
    assert_eq!(unlisted, [&grown]);
}

#[test]
fn a_transition_admits_a_listed_line_only_as_often_as_it_counts() {
    let listed = BTreeMap::from([(("a.rs".to_owned(), 1, 10), 1)]);
    // The listed copy, and a second paste of it further down the same file,
    // which the previous commit already finds unlisted.
    let original = run("a.rs", 1, 10, 100);
    let pasted = run("a.rs", 1, 10, 500);
    let prior = [original.clone(), pasted.clone()];
    let found = [original, pasted.clone()];
    let (unlisted, stale) = judge(&found, Some(&prior), &listed);
    assert_eq!(unlisted, [&pasted]);
    assert!(stale.is_empty(), "{stale:?}");
}

#[test]
fn outside_a_transition_only_a_listed_run_is_admitted() {
    let listed = BTreeMap::from([(("a.rs".to_owned(), 1, 10), 1)]);
    let recut = run("a.rs", 7, 12, 104);
    let (unlisted, stale) = judge(std::slice::from_ref(&recut), None, &listed);
    assert_eq!(unlisted, [&recut]);
    assert_eq!(stale, ["a.rs\t0000000000000001\t10"]);
}

fn sample_layer(digests: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "commit": REFERENCE_COMMIT,
        "uvLockSha256": "0".repeat(64),
        "measuredOn": {"platform": "linux-x86_64", "python": "3.12.15"},
        "distributions": ["httpx2==2.16.0", "pydantic==2.13.4"],
        "digests": digests,
    })
}

#[test]
fn a_run_a_locked_distribution_also_ships_is_not_the_reference_s() {
    let text = "every lantern along the quiet harbor glowed until dawn returned";
    let all = words(text);
    let mut buffer = String::new();
    let shingles: Vec<u64> = (0..=all.len() - WIDTH)
        .map(|start| digest(text, &all[start..start + WIDTH], &mut buffer))
        .collect();
    let mut reference = shingles.clone();
    reference.sort_unstable();
    // A library ships the middle shingle: the 10-word run splits into the
    // two shingles the reference alone holds.
    let shipped = shingles[1];
    let layer = sample_layer(serde_json::json!({ format!("{shipped:016x}"): ["httpx2==2.16.0"] }));
    assert!(layer_problems(&layer, REFERENCE_COMMIT, &reference).is_empty());
    let set = subtract(&reference, &third_party_digests(&layer));
    let found: Vec<(usize, usize)> = runs("sample.md", text, &set)
        .iter()
        .map(|run| (run.start, run.words))
        .collect();
    assert_eq!(found, [(0, 8), (2, 8)]);
    assert!(runs("sample.md", text, &subtract(&reference, &reference)).is_empty());
}

#[test]
fn a_layer_generated_against_something_else_is_refused() {
    let reference = [1_u64, 2, 3];
    let foreign = sample_layer(serde_json::json!({ format!("{:016x}", 9): ["httpx2==2.16.0"] }));
    let unread = sample_layer(serde_json::json!({ format!("{:016x}", 2): ["requests==2.0"] }));
    let mut moved = sample_layer(serde_json::json!({}));
    moved["commit"] = serde_json::json!(PREVIOUS_COMMIT);
    moved["uvLockSha256"] = serde_json::json!("");
    for (layer, expected) in [
        (foreign, "is not a digest of the reference's prose"),
        (unread, "not to distributions the layer read"),
        (moved, "it names the commit"),
    ] {
        let problems = layer_problems(&layer, REFERENCE_COMMIT, &reference);
        assert!(
            problems.iter().any(|problem| problem.contains(expected)),
            "{problems:?}"
        );
    }
}
