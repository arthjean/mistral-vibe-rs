//! `grep`, on the libraries ripgrep is built from.
//!
//! The reference shells out to `rg` with `--smart-case`, `--no-binary`,
//! `--max-count=n+1`, a `!glob` per configured exclusion and `--no-ignore` when
//! the call asks for it, then keeps the first `max_matches` output lines
//! (`vibe/core/tools/builtins/grep.py:262`). This module reaches the same
//! answers through `grep-regex`, `grep-searcher` and `ignore`, which are the
//! crates that binary is assembled from, so the search needs no external
//! program on the host.
//!
//! Two deliberate departures, both recorded in `docs/parity.md`:
//!
//! * The walk is confined to the workspace root. A path outside it is refused
//!   here rather than searched and then reported, which is the confinement every
//!   other file tool in this port already applies.
//! * The matches are ordered by path and then by line. `rg` walks in parallel
//!   and emits whichever file finished first, so its order is not a contract
//!   anything can conform to: the capture that feeds
//!   `tool_execution_parity_tests` recorded two different orders for the same
//!   query in one run. Sorting is the only normalization that makes the answer
//!   comparable, and it is what the oracle now records on both sides.

use std::path::Path;
use std::time::Instant;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

use super::{SearchOptions, Workspace, WorkspaceError, path_display};

/// What one `grep` call answers, in the shape the reference publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepOutcome {
    /// The `path:line:text` lines, joined by newlines and clipped to the
    /// configured output budget.
    pub matches: String,
    /// How many lines survived the match cap, which is what the reference
    /// counts rather than how many the walk found.
    pub match_count: usize,
    /// Whether the match cap or the byte budget cut the answer short.
    pub was_truncated: bool,
}

/// One match, before the cap and the budget apply.
struct Hit {
    path: String,
    line: u64,
    text: String,
}

/// Runs one search over `requested`, resolved against the workspace root.
pub(super) fn run(
    workspace: &Workspace,
    requested: &str,
    pattern: &str,
    options: &SearchOptions,
) -> Result<GrepOutcome, WorkspaceError> {
    if pattern.trim().is_empty() {
        return Err(WorkspaceError::EmptyPattern);
    }
    // The confinement runs first and on the path as written, so a target that
    // escapes the root is named as such rather than reported as missing.
    let relative = workspace
        .confined(Path::new(requested), true)
        .map_err(|error| match error {
            WorkspaceError::Io { .. } => WorkspaceError::MissingSearchPath(requested.to_owned()),
            other => other,
        })?;
    let absolute = workspace.root().join(&relative);
    let is_directory = absolute.is_dir();

    // Compiling before the walk, so an unusable pattern costs no filesystem
    // access at all, which is what the story asks for.
    let matcher = RegexMatcherBuilder::new()
        .case_smart(true)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;

    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    // One past the cap, the way the reference asks `rg` for `--max-count n+1`:
    // the extra line is what tells a full answer from a clipped one.
    let per_file_cap = options.limit.saturating_add(1);

    let mut hits = Vec::new();
    if is_directory {
        let excludes = exclusions(workspace, options);
        let mut overrides = OverrideBuilder::new(&absolute);
        for pattern in &excludes {
            overrides
                .add(&format!("!{pattern}"))
                .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
        }
        let overrides = overrides
            .build()
            .map_err(|error| WorkspaceError::InvalidPattern(error.to_string()))?;
        let honor_ignore = options.use_default_ignore;
        let walk = WalkBuilder::new(&absolute)
            .overrides(overrides)
            .standard_filters(false)
            // `rg` skips hidden entries whether or not `--no-ignore` was
            // passed: only the ignore files stop being read.
            .hidden(true)
            .parents(honor_ignore)
            .ignore(honor_ignore)
            .git_ignore(honor_ignore)
            .git_global(honor_ignore)
            .git_exclude(honor_ignore)
            .build();
        for entry in walk {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(WorkspaceError::SearchTimeout {
                    seconds: options.timeout.unwrap_or_default().as_secs(),
                });
            }
            // A walk error is skipped rather than raised: `rg` reports an
            // unreadable entry on stderr and keeps searching.
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let display = match entry.path().strip_prefix(&absolute) {
                Ok(inside) => joined_display(requested, inside),
                Err(_) => path_display(entry.path()),
            };
            collect(&matcher, entry.path(), &display, per_file_cap, &mut hits)?;
        }
    } else {
        collect(
            &matcher,
            &absolute,
            &normalized(requested),
            per_file_cap,
            &mut hits,
        )?;
    }

    hits.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.line.cmp(&right.line))
    });
    Ok(assemble(&hits, options))
}

/// The `path:line:text` lines, capped and clipped the way the reference caps
/// and clips them.
fn assemble(hits: &[Hit], options: &SearchOptions) -> GrepOutcome {
    let lines = hits
        .iter()
        .map(|hit| format!("{}:{}:{}", hit.path, hit.line, hit.text))
        .collect::<Vec<_>>();
    let kept = lines.get(..options.limit).unwrap_or(&lines);
    let joined = kept.join("\n");
    // The reference counts characters here rather than bytes, because it slices
    // a Python string; the flag and the clip therefore agree on the same unit.
    let rendered = joined.chars().count();
    let was_truncated = lines.len() > options.limit || rendered > options.max_output_bytes;
    let matches = if rendered > options.max_output_bytes {
        joined.chars().take(options.max_output_bytes).collect()
    } else {
        joined
    };
    GrepOutcome {
        matches,
        match_count: kept.len(),
        was_truncated,
    }
}

/// Every match in one file, up to `cap`.
fn collect(
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    display: &str,
    cap: usize,
    into: &mut Vec<Hit>,
) -> Result<(), WorkspaceError> {
    let mut found = 0_usize;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let outcome = searcher.search_path(
        matcher,
        path,
        UTF8(|line, text| {
            found = found.saturating_add(1);
            into.push(Hit {
                path: display.to_owned(),
                line,
                // `rg` strips the line terminator and nothing else, so a
                // carriage return stays part of the matched line.
                text: text.strip_suffix('\n').unwrap_or(text).to_owned(),
            });
            Ok(found < cap)
        }),
    );
    // An unreadable or undecodable file is skipped, which is what `rg` does
    // with the same file: the search answers about the rest of the tree.
    let _ = outcome;
    Ok(())
}

/// The configured exclusions plus whatever the codeignore file names.
fn exclusions(workspace: &Workspace, options: &SearchOptions) -> Vec<String> {
    let mut patterns = options
        .exclude_patterns
        .iter()
        .filter(|pattern| !pattern.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();
    if options.codeignore_file.is_empty() {
        return patterns;
    }
    // Read from the working directory, which is where the reference looks for
    // it, rather than from the directory the call happens to be scoped to.
    let codeignore = workspace.root().join(&options.codeignore_file);
    if let Ok(content) = std::fs::read_to_string(&codeignore) {
        patterns.extend(
            content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned),
        );
    }
    patterns
}

/// The path argument as given, with the walked remainder joined onto it.
///
/// `rg` prints what it was handed plus the relative path underneath, which is
/// why a search of `.` answers `./nested/beta.py` and a search of an absolute
/// directory answers an absolute path.
fn joined_display(requested: &str, inside: &Path) -> String {
    if inside.as_os_str().is_empty() {
        return normalized(requested);
    }
    let mut joined = requested.trim_end_matches(['/', '\\']).to_owned();
    joined.push('/');
    joined.push_str(&inside.to_string_lossy().replace('\\', "/"));
    joined
}

fn normalized(requested: &str) -> String {
    requested.replace('\\', "/")
}
