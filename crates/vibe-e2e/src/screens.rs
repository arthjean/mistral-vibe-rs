//! Normalized screens and the keystroke scripts that produce them: the
//! black-box terminal-client oracle of `tasks/prd-trustworthy-parity-v3.md`
//! (US-019, US-020).
//!
//! A [`Script`] is a list of keys, resizes, waits and snapshots. [`play`] runs
//! it against one subject on a pseudo-terminal and returns what each snapshot
//! saw as a [`normalized`] screen: the row count, each row as its text when
//! every letter on it is scenario-authored and as a length and a digest
//! otherwise, the attribute runs reduced to classes, the cursor, the alternate
//! screen flag and the window titles. No other text of the subject is kept,
//! so a corpus captured from the reference holds no row it authored.
//!
//! A cell that varies between two runs of one script is masked, never
//! tolerated by a ledger: a [`Masks`] names the character class it replaces
//! and why.
//!
//! # Measured terminal-client facts (US-019, 2026-11-09)
//!
//! The v3.1.0 reference (`c96a6db`) starts its Rust client, `vibe-rs`, from
//! the `vibe` console script (`vibe/cli/launcher.py`), which points it at the
//! bundled `vibe-app-server` through `VIBE_APP_SERVER_BIN`. Both ran in a
//! [`crate::World`] with the loopback platform as the provider, on a 120 by
//! 40 terminal, on Ubuntu 26.04 under WSL 2 (kernel 6.18, Linux PTY) and on Windows
//! 11 26300 (ConPTY):
//!
//! | Fact | Linux PTY | ConPTY |
//! |------|-----------|--------|
//! | composer ready (model and `/help` shown), three starts | 2.40, 2.41, 2.47 s | 2.82, 2.80, 2.89 s |
//! | eight startup and composer scripts, five runs each, masked | identical | identical |
//! | what varied before the masks below | the logo's braille and the blanks between them (rows 1 and 3), the order of the session title and the idle title, the footer's padding (it right-aligns against the working directory, whose length followed the world's name) | the same, the hidden cursor's position, and the drawn cursor cell |
//! | window titles (OSC 0 and 2) through `vt100::Callbacks` | `Vibe`, `>> Vibe` while a turn runs, the session title | ConPTY first sets the program path, then the same |
//! | app-server methods through `VIBE_APP_SERVER_CMD` and the relay, one turn | 17 client requests, `initialize` first, `session/turn/enqueue` among them | the same requests up to the turn |
//!
//! Three properties shape the runner. The startup logo is an animation of
//! braille cells that never stops, and the composer draws its own blinking
//! cursor by inverting a cell while the terminal cursor stays hidden, so a
//! screen never goes quiet byte-wise: [`Masks::default`] replaces every
//! braille cell and the blanks around it with one class character, drops the
//! attributes of a cell drawn inverted over palette color 0, and records no
//! position for a hidden cursor (ConPTY leaves it wherever its repaint
//! ended); a settle waits for the masked screen to stop changing, and the
//! world's name has a fixed width. The client reads a burst of
//! keys that ends in Enter as a paste and inserts a newline (`key_burst.rs`
//! in `vibe/cli-rust/src/`), so [`Step::Submit`] settles before it presses
//! Enter. On Windows the Python launcher needs `USERPROFILE`, which the world
//! sets, or `Path.home()` fails and the client reports the app-server closed.
//!
//! The fallback the risk table names, the app-server method sequence, is
//! recorded by starting `vibe-rs` itself with `VIBE_APP_SERVER_CMD` naming a
//! relay (`scripts/parity/app_server_tap.py`). No step failed on either
//! platform, so the fallback replaces the screen oracle on neither: it stays
//! a probe of the protocol the client issues. The corpora are captured on
//! Linux, where every live probe runs.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::process::Command;
use crate::pty::{Key, Screen, Tui};
use crate::world::World;
use crate::{Failure, scaled};

/// How long a masked screen stays unchanged before a settle accepts it.
pub const QUIET: Duration = Duration::from_millis(900);

/// How long one wait or settle may take before the script fails, before
/// scaling.
pub const STEP_DEADLINE: Duration = Duration::from_secs(30);

/// The character a masked cell reads as.
pub const MASKED: char = '\u{2591}';

/// One step of a [`Script`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Types scenario-authored text, which the normalized rows may then
    /// show verbatim.
    Type(&'static str),
    /// Sends keys.
    Keys(Vec<Key>),
    /// Settles, then presses Enter, so the Enter never joins a key burst.
    Submit,
    /// Resizes the terminal.
    Resize(u16, u16),
    /// Waits until `text` appears on one row.
    WaitFor(&'static str),
    /// Waits until no row shows `text`, for a state a masked spinner would
    /// make look settled.
    WaitGone(&'static str),
    /// Waits until the masked screen stops changing.
    Settle,
    /// Sleeps, for a step whose effect has no visible end.
    Pause(u64),
    /// Settles and records the screen under a name.
    Snapshot(&'static str),
    /// Waits for the subject to exit and records its code and final screen.
    Exit(&'static str),
}

impl Step {
    /// The step as a corpus records it, so a replay knows both subjects ran
    /// the same script.
    #[must_use]
    pub fn describe(&self) -> Value {
        match self {
            Self::Type(text) => json!({"type": text}),
            Self::Keys(keys) => json!({"keys": keys.iter().map(describe_key).collect::<Vec<_>>()}),
            Self::Submit => json!("submit"),
            Self::Resize(cols, rows) => json!({"resize": [cols, rows]}),
            Self::WaitFor(text) => json!({"waitFor": text}),
            Self::WaitGone(text) => json!({"waitGone": text}),
            Self::Settle => json!("settle"),
            Self::Pause(milliseconds) => json!({"pause": milliseconds}),
            Self::Snapshot(name) => json!({"snapshot": name}),
            Self::Exit(name) => json!({"exit": name}),
        }
    }
}

fn describe_key(key: &Key) -> String {
    match key {
        Key::Text(text) => format!("text:{text}"),
        Key::Paste(text) => format!("paste:{text}"),
        Key::Ctrl(letter) => format!("ctrl+{letter}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// A keystroke script played against one subject.
#[derive(Debug, Clone)]
pub struct Script {
    /// The scenario name a corpus keys the script under, `family/name`.
    pub name: &'static str,
    /// The terminal it starts on.
    pub size: (u16, u16),
    /// The arguments the subject starts with.
    pub arguments: Vec<&'static str>,
    /// Text the scenario authors besides what it types: stand-in answers,
    /// seeded file names, the model alias. A row made only of these keeps its
    /// text.
    pub authored: Vec<&'static str>,
    pub steps: Vec<Step>,
}

impl Script {
    /// The script as a corpus records it.
    #[must_use]
    pub fn describe(&self) -> Value {
        json!({
            "size": [self.size.0, self.size.1],
            "arguments": self.arguments,
            "steps": self.steps.iter().map(Step::describe).collect::<Vec<_>>(),
        })
    }

    /// Every string the scenario authored: what it types and what it
    /// declares.
    fn authored(&self) -> Vec<&'static str> {
        let typed = self.steps.iter().filter_map(|step| match step {
            Step::Type(text) => Some(*text),
            _ => None,
        });
        let mut authored: Vec<&'static str> = typed.chain(self.authored.iter().copied()).collect();
        // Longest first, so a string that contains another is removed whole.
        authored.sort_by_key(|text| std::cmp::Reverse(text.chars().count()));
        authored
    }
}

/// What normalization erases, each with the reason a corpus records.
#[derive(Debug, Clone)]
pub struct Masks {
    /// Character ranges replaced by [`MASKED`].
    pub ranges: Vec<(char, char, &'static str)>,
    /// Whether a cell drawn inverted over palette color 0 loses its
    /// attributes: the composer draws its blinking cursor so.
    pub drawn_cursor: Option<&'static str>,
    /// Literal text replaced by a token, such as the world's root.
    pub tokens: Vec<(String, &'static str)>,
}

impl Default for Masks {
    fn default() -> Self {
        Self {
            ranges: vec![(
                '\u{2800}',
                '\u{28ff}',
                "braille cells animate: the startup logo never stops and spinners turn",
            )],
            drawn_cursor: Some(
                "the composer blinks its own cursor by drawing a cell inverted over palette color \
                 0, wherever the hidden terminal cursor is left",
            ),
            tokens: Vec::new(),
        }
    }
}

impl Masks {
    /// These masks, with the world's directories replaced by tokens in both
    /// separator spellings.
    #[must_use]
    pub fn for_world(mut self, world: &World) -> Self {
        let mut add = |path: std::path::PathBuf, token: &'static str| {
            let text = path.to_string_lossy().into_owned();
            self.tokens.push((text.replace('\\', "/"), token));
            self.tokens.push((text.replace('/', "\\"), token));
            self.tokens.push((text, token));
        };
        add(world.work(), "<work>");
        add(world.home(), "<home>");
        add(world.root().to_path_buf(), "<world>");
        // The loopback platform listens on a port of its own every run.
        self.tokens
            .push((world.platform().base().to_owned(), "<platform>"));
        self.tokens
            .sort_by_key(|(text, _)| std::cmp::Reverse(text.len()));
        self.tokens.dedup();
        self
    }

    /// The masks as a corpus records them.
    #[must_use]
    pub fn describe(&self) -> Value {
        let tokens: BTreeSet<&str> = self.tokens.iter().map(|(_, token)| *token).collect();
        json!({
            "ranges": self.ranges.iter().map(|(from, to, why)| json!({
                "from": format!("U+{:04X}", u32::from(*from)),
                "to": format!("U+{:04X}", u32::from(*to)),
                "reason": why,
            })).collect::<Vec<_>>(),
            "drawnCursor": self.drawn_cursor,
            "tokens": tokens,
        })
    }

    fn masks(&self, character: char) -> bool {
        self.ranges
            .iter()
            .any(|(from, to, _)| (*from..=*to).contains(&character))
    }

    fn tokenize(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (literal, token) in &self.tokens {
            if !literal.is_empty() {
                text = text.replace(literal.as_str(), token);
            }
        }
        // A token followed by a path keeps one separator spelling.
        for token in ["<work>", "<home>", "<world>"] {
            let mut rest = text.as_str();
            let mut out = String::new();
            while let Some(index) = rest.find(token) {
                out.push_str(&rest[..index + token.len()]);
                rest = &rest[index + token.len()..];
                let path_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                out.push_str(&rest[..path_end].replace('\\', "/"));
                rest = &rest[path_end..];
            }
            out.push_str(rest);
            text = out;
        }
        text
    }
}

/// The attribute class of one cell: foreground and background kinds, bold
/// and inverse.
fn class(cell: &vt100::Cell) -> Option<String> {
    let kind = |color: vt100::Color| match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(_) => Some("palette"),
        vt100::Color::Rgb(..) => Some("rgb"),
    };
    let mut parts = Vec::new();
    if let Some(kind) = kind(cell.fgcolor()) {
        parts.push(format!("fg:{kind}"));
    }
    if let Some(kind) = kind(cell.bgcolor()) {
        parts.push(format!("bg:{kind}"));
    }
    if cell.bold() {
        parts.push("bold".to_owned());
    }
    if cell.inverse() {
        parts.push("inverse".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The row's text with masked cells replaced, trailing blanks trimmed, and
/// which columns were masked. A masked cell takes the blank cells around it,
/// up to the nearest other character, with it: an animation that moves its
/// blanks then reads the same.
fn masked_row(screen: &Screen, row: u16, masks: &Masks) -> (String, Vec<bool>) {
    let (cols, _) = screen.size();
    // Every column, its contents ("" for a wide character's second half) and
    // whether a range masks it.
    let cells: Vec<(String, bool, bool)> = (0..cols)
        .map(|col| match screen.cell(row, col) {
            Some(cell) if cell.is_wide_continuation() => (String::new(), false, true),
            Some(cell) => {
                let contents = cell.contents();
                let masked = contents.chars().any(|c| masks.masks(c));
                let shown = if contents.is_empty() { " " } else { contents };
                (shown.to_owned(), masked, false)
            }
            None => (" ".to_owned(), false, false),
        })
        .collect();
    let spreads = |index: usize| {
        let (contents, masked, continuation) = &cells[index];
        *masked || (!continuation && contents.trim().is_empty())
    };
    let mut masked: Vec<bool> = vec![false; cells.len()];
    for index in (0..cells.len()).filter(|index| cells[*index].1) {
        let mut left = index;
        while left > 0 && spreads(left - 1) {
            left -= 1;
        }
        let mut right = index;
        while right + 1 < cells.len() && spreads(right + 1) {
            right += 1;
        }
        masked[left..=right].fill(true);
    }
    let text: String = cells
        .iter()
        .zip(&masked)
        .filter(|((_, _, continuation), _)| !continuation)
        .map(|((contents, _, _), masked)| {
            if *masked {
                MASKED.to_string()
            } else {
                contents.clone()
            }
        })
        .collect();
    (text.trim_end().to_owned(), masked)
}

/// Whether every letter or digit of `text` belongs to a scenario-authored
/// string.
fn authored_only(text: &str, authored: &[&str]) -> bool {
    let mut rest = text.to_owned();
    for piece in authored {
        if !piece.is_empty() {
            rest = rest.replace(piece, " ");
        }
    }
    for token in ["<work>", "<home>", "<world>", "<platform>"] {
        rest = rest.replace(token, " ");
    }
    !rest.chars().any(char::is_alphanumeric)
}

/// The SHA-256 of `text`, hex.
fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// One screen as a corpus records it. `titles` are the window titles set so
/// far, kept as a set; a title naming the program, which ConPTY sets before
/// the client starts, is dropped.
#[must_use]
pub fn normalized(
    screen: &Screen,
    titles: &[String],
    authored: &[&str],
    masks: &Masks,
    program: &str,
) -> Value {
    let (cols, rows) = screen.size();
    let mut texts = Vec::new();
    let mut attributes = Vec::new();
    for row in 0..rows {
        let (text, masked) = masked_row(screen, row, masks);
        let text = masks.tokenize(&text);
        texts.push(if authored_only(&text, authored) {
            json!({"text": text})
        } else {
            json!({"length": text.chars().count(), "sha256": sha256(&text)})
        });
        let mut runs: Vec<(u16, u16, String)> = Vec::new();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let drawn_cursor = masks.drawn_cursor.is_some()
                && cell.inverse()
                && cell.bgcolor() == vt100::Color::Idx(0);
            let hidden = drawn_cursor || masked.get(usize::from(col)).copied().unwrap_or(false);
            let class = if hidden { None } else { class(cell) };
            let Some(class) = class else { continue };
            match runs.last_mut() {
                Some((start, length, last)) if *last == class && *start + *length == col => {
                    *length += 1;
                }
                _ => runs.push((col, 1, class)),
            }
        }
        if !runs.is_empty() {
            attributes.push(json!({
                "row": row,
                "runs": runs.into_iter().map(|(col, length, class)| json!([col, length, class])).collect::<Vec<_>>(),
            }));
        }
    }
    // Two titles set together race (the session title and the idle title
    // after a turn), so the titles read as the set seen so far.
    let kept: BTreeSet<String> = titles
        .iter()
        .map(|title| masks.tokenize(title))
        .filter(|title| {
            program.is_empty()
                || (*title != program && title.replace('\\', "/") != program.replace('\\', "/"))
        })
        .collect();
    json!({
        "size": [cols, rows],
        "alternate": screen.alternate(),
        "cursor": cursor(screen),
        "rows": texts,
        "attributes": attributes,
        "titles": kept,
    })
}

/// Where the terminal cursor is, when it is shown. A hidden cursor's
/// position is no part of what the user sees, and ConPTY leaves it wherever
/// its last repaint ended.
fn cursor(screen: &Screen) -> Value {
    if screen.cursor_visible() {
        let (row, col) = screen.cursor();
        json!({"row": row, "col": col, "visible": true})
    } else {
        json!({"visible": false})
    }
}

/// A script that stopped before its end, with the last screen it saw.
#[derive(Debug, Clone)]
pub struct Stalled {
    pub message: String,
    /// The index of the snapshot the script was heading for.
    pub snapshot: usize,
    /// The last normalized screen.
    pub screen: Value,
}

impl From<Stalled> for Failure {
    fn from(stalled: Stalled) -> Self {
        Self(stalled.message)
    }
}

/// Plays `script` on `command` (the subject's binary in `world`) and returns
/// what it observed: one entry per snapshot, in order.
///
/// # Errors
///
/// [`Stalled`] when a wait or settle does not end within its deadline, or the
/// subject cannot start.
pub fn play(
    world: &World,
    command: Command,
    script: &Script,
    masks: &Masks,
) -> Result<Value, Stalled> {
    let masks = masks.clone().for_world(world);
    let authored = script.authored();
    let program = command.program().to_string_lossy().into_owned();
    let stalled = |message: String, snapshot: usize, screen: Value| Stalled {
        message,
        snapshot,
        screen,
    };
    let mut tui = command
        .args(script.arguments.iter().copied())
        .spawn_tui(script.size.0, script.size.1)
        .map_err(|Failure(message)| stalled(message, 0, Value::Null))?;
    let view = |tui: &Tui| normalized(&tui.screen(), &tui.titles(), &authored, &masks, &program);
    let mut snapshots = Vec::new();
    for step in &script.steps {
        let fail = |tui: &Tui, message: String, snapshots: &Vec<Value>| {
            stalled(
                format!("{}: {message}", script.name),
                snapshots.len(),
                view(tui),
            )
        };
        match step {
            Step::Type(text) => tui
                .type_text(text)
                .map_err(|Failure(m)| fail(&tui, m, &snapshots))?,
            Step::Keys(keys) => tui
                .send(keys)
                .map_err(|Failure(m)| fail(&tui, m, &snapshots))?,
            Step::Submit => {
                settle(&mut tui, &view).map_err(|m| fail(&tui, m, &snapshots))?;
                tui.send(&[Key::Enter])
                    .map_err(|Failure(m)| fail(&tui, m, &snapshots))?;
            }
            Step::Resize(cols, rows) => tui
                .resize(*cols, *rows)
                .map_err(|Failure(m)| fail(&tui, m, &snapshots))?,
            Step::WaitFor(text) => {
                tui.wait_text(text, STEP_DEADLINE)
                    .map_err(|Failure(m)| fail(&tui, m, &snapshots))?;
            }
            Step::WaitGone(text) => {
                tui.wait_screen(
                    &format!("no {text:?}"),
                    |screen| !screen.contains(text),
                    STEP_DEADLINE,
                )
                .map_err(|Failure(m)| fail(&tui, m, &snapshots))?;
            }
            Step::Settle => {
                settle(&mut tui, &view).map_err(|m| fail(&tui, m, &snapshots))?;
            }
            Step::Pause(milliseconds) => std::thread::sleep(Duration::from_millis(*milliseconds)),
            Step::Snapshot(name) => {
                let screen = settle(&mut tui, &view).map_err(|m| fail(&tui, m, &snapshots))?;
                snapshots.push(json!({"name": name, "screen": screen}));
            }
            Step::Exit(name) => {
                let exit = tui
                    .wait_exit(STEP_DEADLINE)
                    .map_err(|Failure(m)| fail(&tui, m, &snapshots))?;
                snapshots.push(json!({"name": name, "exit": exit.code, "screen": view(&tui)}));
            }
        }
    }
    tui.kill();
    Ok(json!({"snapshots": snapshots}))
}

/// Waits until the masked screen stays the same for [`QUIET`], and returns
/// it.
fn settle(tui: &mut Tui, view: &impl Fn(&Tui) -> Value) -> Result<Value, String> {
    let deadline = scaled(STEP_DEADLINE);
    let started = Instant::now();
    let mut last = view(tui);
    let mut since = Instant::now();
    let mut changed = None;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now = view(tui);
        if now == last {
            if since.elapsed() >= QUIET {
                return Ok(now);
            }
        } else {
            changed = first_difference(&last, &now);
            last = now;
            since = Instant::now();
        }
        if started.elapsed() >= deadline {
            return Err(format!(
                "the masked screen kept changing for {deadline:?}; declare a mask for the cells \
                 that vary (the last change: {})",
                changed.as_deref().unwrap_or("none recorded")
            ));
        }
    }
}

/// The first place two normalized screens differ, in words a maintainer can
/// act on.
#[must_use]
pub fn first_difference(expected: &Value, actual: &Value) -> Option<String> {
    for key in ["size", "alternate", "cursor", "titles"] {
        if expected.get(key) != actual.get(key) {
            return Some(format!(
                "{key}: expected {}, found {}",
                expected.get(key).unwrap_or(&Value::Null),
                actual.get(key).unwrap_or(&Value::Null)
            ));
        }
    }
    let rows = |screen: &Value| screen["rows"].as_array().cloned().unwrap_or_default();
    let (left, right) = (rows(expected), rows(actual));
    for (index, (a, b)) in left.iter().zip(&right).enumerate() {
        if a == b {
            continue;
        }
        let column = match (a["text"].as_str(), b["text"].as_str()) {
            (Some(a), Some(b)) => a
                .chars()
                .zip(b.chars())
                .position(|(x, y)| x != y)
                .unwrap_or_else(|| a.chars().count().min(b.chars().count()))
                .to_string(),
            _ => "unknown (a digested row)".to_owned(),
        };
        return Some(format!(
            "row {index}, column {column}: expected {a}, found {b}"
        ));
    }
    (left.len() != right.len())
        .then(|| format!("{} rows expected, {} found", left.len(), right.len()))
        .or_else(|| {
            let attributes =
                |screen: &Value| screen["attributes"].as_array().cloned().unwrap_or_default();
            let (left, right) = (attributes(expected), attributes(actual));
            left.iter()
                .zip(&right)
                .find(|(a, b)| a != b)
                .map(|(a, b)| format!("attributes: expected {a}, found {b}"))
                .or_else(|| (expected != actual).then(|| "the attribute rows differ".to_owned()))
        })
}

/// A normalized screen as lines a failure message quotes.
#[must_use]
pub fn render(screen: &Value) -> String {
    let mut out = String::new();
    for (index, row) in screen["rows"].as_array().into_iter().flatten().enumerate() {
        let line = row["text"].as_str().map_or_else(
            || {
                format!(
                    "<{} characters, {}>",
                    row["length"],
                    &row["sha256"].as_str().unwrap_or("")
                        [..12.min(row["sha256"].as_str().unwrap_or("").len())]
                )
            },
            str::to_owned,
        );
        let _ = writeln!(out, "{index:02}|{line}");
    }
    out
}

#[cfg(test)]
mod screens_tests;
