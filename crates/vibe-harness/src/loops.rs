//! The prompts a unified session schedules for itself.
//!
//! Reference `vibe/app_server/_unified_scheduled_loops.py` (the schedule and
//! its `scheduled-loops.json`), `vibe/app_server/_cron.py` (the `cron` tool)
//! and `vibe/core/loop.py` (`parse_interval` and the limits). A loop repeats
//! on an interval or on a five-field cron expression read in the machine's
//! time zone; it runs only while its session is live and idle, and a run
//! missed while it was not is not caught up.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use vibe_core::atomic_file::{FileMode, write_atomically};
use vibe_core::pycompat::{self, Dumps, pydantic};

use crate::state::ToolResult;

/// Reference `MIN_INTERVAL_SECONDS` and `MAX_LOOPS_PER_SESSION`.
pub const MIN_INTERVAL_SECONDS: u64 = 30;
pub const MAX_LOOPS_PER_SESSION: usize = 50;

const FORMAT: &str = "vibe.scheduled-loops/v1";
/// Reference `croniter(..., max_years_between_matches=50)`.
const MAX_YEARS_BETWEEN_MATCHES: i64 = 50;

/// One scheduled prompt. Reference `ScheduledPrompt`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledPrompt {
    pub id: String,
    pub interval_seconds: Option<u64>,
    pub prompt: String,
    pub next_fire_at: f64,
    pub created_at: f64,
    pub cron: Option<String>,
}

impl ScheduledPrompt {
    /// The model dump the `cron` tool answers with: `interval_seconds`
    /// always, `cron` only when set.
    #[must_use]
    pub fn dump(&self) -> Value {
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(self.id));
        value.insert("interval_seconds".to_owned(), json!(self.interval_seconds));
        value.insert("prompt".to_owned(), json!(self.prompt));
        value.insert("next_fire_at".to_owned(), json!(self.next_fire_at));
        value.insert("created_at".to_owned(), json!(self.created_at));
        if let Some(cron) = &self.cron {
            value.insert("cron".to_owned(), json!(cron));
        }
        Value::Object(value)
    }

    /// What `scheduled-loops.json` stores: the dump without its nulls.
    fn stored(&self) -> Value {
        let mut value = self.dump();
        if let Some(map) = value.as_object_mut() {
            map.retain(|_, item| !item.is_null());
        }
        value
    }

    fn from_stored(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            interval_seconds: value.get("interval_seconds").and_then(Value::as_u64),
            prompt: value.get("prompt")?.as_str()?.to_owned(),
            next_fire_at: value.get("next_fire_at")?.as_f64()?,
            created_at: value.get("created_at")?.as_f64()?,
            cron: value
                .get("cron")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    /// The protocol's `ScheduledLoop`: `loops/*` answers with it.
    #[must_use]
    pub fn public(&self) -> Value {
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(self.id));
        value.insert("prompt".to_owned(), json!(self.prompt));
        if let Some(interval) = self.interval_seconds {
            value.insert("intervalSeconds".to_owned(), json!(interval));
        }
        if let Some(cron) = &self.cron {
            value.insert("cron".to_owned(), json!(cron));
        }
        value.insert("nextFireAt".to_owned(), json!(self.next_fire_at));
        Value::Object(value)
    }

    fn next_fire_after(&self, now: f64) -> Result<f64, String> {
        match (self.interval_seconds, &self.cron) {
            #[allow(clippy::cast_precision_loss)]
            (Some(interval), _) => Ok(now + interval as f64),
            (None, Some(cron)) => next_cron_fire(cron, now),
            (None, None) => Err("Exactly one of interval_seconds or cron is required.".to_owned()),
        }
    }
}

/// What a `/loop` prompt carries to the model besides the user's words,
/// and what the public history never shows. Reference `LOOP_INSTRUCTIONS`
/// in `vibe/app_server/_loop_prompt.py`, in this port's own words.
pub const LOOP_INSTRUCTIONS: &str = "<loop_instructions>\n\
`/loop [schedule] [prompt]` asks for a prompt that repeats. Read the schedule as plain \
language and call the cron tool with it: interval_seconds for a fixed frequency, or a \
five-field cron expression in the machine's local time zone for a calendar schedule. \
Schedule the prompt; do not run it now. The same tool lists and cancels schedules when \
asked. When the request is unclear, ask the user.\n\
</loop_instructions>";

/// Reference `is_loop_prompt`: the first word is `/loop`, in any case.
#[must_use]
pub fn is_loop_prompt(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|word| word.to_lowercase() == "/loop")
}

fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reference `prepare_loop_turn`: a `/loop` prompt gains the instructions.
pub fn prepare_loop_content(content: &mut Vec<Value>) {
    if is_loop_prompt(&text_of(content)) {
        content.push(json!({"type": "text", "text": LOOP_INSTRUCTIONS}));
    }
}

/// Reference `_without_instructions`: the content a client sees.
#[must_use]
pub fn without_instructions(content: &Value) -> Value {
    let Some(blocks) = content.as_array() else {
        return content.clone();
    };
    if !is_loop_prompt(&text_of(blocks)) {
        return content.clone();
    }
    Value::Array(
        blocks
            .iter()
            .filter(|block| !(block["type"] == "text" && block["text"] == LOOP_INSTRUCTIONS))
            .cloned()
            .collect(),
    )
}

/// Reference `project_loop_preview`: a `/loop` preview without the
/// instructions the harness flattened into it, cut or whole.
#[must_use]
pub fn project_loop_preview(preview: &str) -> String {
    if !is_loop_prompt(preview) {
        return preview.to_owned();
    }
    let instructions = LOOP_INSTRUCTIONS.replace('\n', " ");
    let mut end = preview.len();
    while let Some(start) = preview[..end].rfind(" <") {
        let candidate = &preview[start + 1..];
        let cut = candidate
            .strip_suffix('…')
            .is_some_and(|head| instructions.starts_with(head));
        if candidate == instructions || cut {
            return preview[..start].trim_end().to_owned();
        }
        end = start;
    }
    preview.to_owned()
}

/// A loop failure, worded as the reference words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopError(pub String);

/// The schedule of one session.
#[derive(Debug, Default)]
pub struct ScheduledLoops {
    state: Mutex<Schedule>,
}

#[derive(Debug, Default)]
struct Schedule {
    loops: Vec<ScheduledPrompt>,
    /// Where the schedule is saved once its session is.
    path: Option<PathBuf>,
    /// Whether loops can be scheduled: a headless session cannot.
    disabled: bool,
}

/// Reference `parse_interval`.
///
/// # Errors
///
/// An interval that is missing, malformed or shorter than the minimum.
pub fn parse_interval(text: &str) -> Result<u64, LoopError> {
    if text.is_empty() {
        return Err(LoopError("Missing interval.".to_owned()));
    }
    let normalized = text.trim().to_lowercase();
    let invalid = || {
        LoopError(format!(
            "Invalid interval `{text}`. Expected: <number><unit> (e.g., 30s, 5m, 2h, 1d)."
        ))
    };
    let unit = normalized.chars().last().ok_or_else(invalid)?;
    let digits = &normalized[..normalized.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(invalid()),
    };
    let seconds = digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(invalid)?;
    if seconds < MIN_INTERVAL_SECONDS {
        return Err(LoopError(format!(
            "Interval must be at least {MIN_INTERVAL_SECONDS}s."
        )));
    }
    Ok(seconds)
}

/// A pydantic `value_error` raised by `ScheduledPrompt.validate_prompt`,
/// which reports the field and the string it was given.
fn prompt_error(message: &str, prompt: &str) -> LoopError {
    LoopError(pydantic::validation_error(
        "ScheduledPrompt",
        &[pydantic::LineError {
            location: "prompt",
            kind: "value_error",
            message: &format!("Value error, {message}"),
            input: &json!(prompt),
        }],
    ))
}

/// A pydantic `value_error` on the whole `ScheduledPrompt` being built, as
/// the reference raises it: the input is the dict of every field.
fn model_error(message: &str, candidate: &ScheduledPrompt) -> LoopError {
    let input = json!({
        "id": candidate.id,
        "interval_seconds": candidate.interval_seconds,
        "cron": candidate.cron,
        "prompt": candidate.prompt,
        "next_fire_at": candidate.next_fire_at,
        "created_at": candidate.created_at,
    });
    LoopError(pydantic::validation_error(
        "ScheduledPrompt",
        &[pydantic::LineError {
            location: "",
            kind: "value_error",
            message: &format!("Value error, {message}"),
            input: &input,
        }],
    ))
}

impl ScheduledLoops {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns scheduling off, as a headless session has it.
    pub fn disable(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.disabled = true;
        }
    }

    /// Saves the schedule at `path` from now on, starting with what it
    /// holds. Reference `_persist_scheduled_loops_after_promotion`.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn attach(&self, path: PathBuf) -> Result<(), LoopError> {
        let mut state = self.lock()?;
        state.path = Some(path);
        let loops = state.loops.clone();
        persist(&state, &loops)
    }

    /// Reads the schedule `path` holds and saves there from now on.
    /// Reference `restore`.
    ///
    /// # Errors
    ///
    /// A file that exists but does not parse.
    pub fn restore(&self, path: PathBuf) -> Result<(), LoopError> {
        let mut state = self.lock()?;
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                LoopError(format!(
                    "Failed to read scheduled loops at {}: {error}",
                    path.display()
                ))
            })?;
            let value: Value = serde_json::from_str(&text).map_err(|error| {
                LoopError(format!(
                    "Failed to read scheduled loops at {}: {error}",
                    path.display()
                ))
            })?;
            let loops = value
                .get("loops")
                .and_then(Value::as_array)
                .map(|loops| {
                    loops
                        .iter()
                        .filter_map(ScheduledPrompt::from_stored)
                        .collect()
                })
                .unwrap_or_default();
            state.loops = loops;
        }
        state.path = Some(path);
        Ok(())
    }

    /// Replaces the schedule, as a fork copies its source's.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn replace(&self, loops: Vec<ScheduledPrompt>) -> Result<(), LoopError> {
        let mut state = self.lock()?;
        persist(&state, &loops)?;
        state.loops = loops;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Schedule>, LoopError> {
        self.state
            .lock()
            .map_err(|_| LoopError("the schedule is unavailable".to_owned()))
    }

    #[must_use]
    pub fn list(&self) -> Vec<ScheduledPrompt> {
        self.state
            .lock()
            .map(|state| state.loops.clone())
            .unwrap_or_default()
    }

    /// Reference `create`: an interval in `<number><unit>` form.
    ///
    /// # Errors
    ///
    /// A bad interval or prompt, a full schedule, or a failed save.
    pub fn create(&self, interval: &str, prompt: &str) -> Result<ScheduledPrompt, LoopError> {
        let seconds = parse_interval(interval)?;
        self.create_with(Some(seconds), None, prompt)
    }

    /// Reference `_create`.
    ///
    /// # Errors
    ///
    /// As [`Self::create`].
    pub fn create_with(
        &self,
        interval_seconds: Option<u64>,
        cron: Option<&str>,
        prompt: &str,
    ) -> Result<ScheduledPrompt, LoopError> {
        let mut state = self.lock()?;
        if state.loops.len() >= MAX_LOOPS_PER_SESSION {
            return Err(LoopError(format!(
                "Loop limit reached ({MAX_LOOPS_PER_SESSION} per session)."
            )));
        }
        let now = vibe_core::clock::now_seconds_f64();
        let mut candidate = ScheduledPrompt {
            id: vibe_core::session_id::random_hex::<4>(),
            interval_seconds,
            prompt: prompt.to_owned(),
            next_fire_at: now,
            created_at: now,
            cron: cron.map(ToOwned::to_owned),
        };
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Err(prompt_error("Missing prompt.", prompt));
        }
        if trimmed.starts_with('/') {
            return Err(prompt_error("Prompt cannot start with '/'.", prompt));
        }
        if let Some(cron) = cron {
            next_cron_fire(cron.trim(), now)
                .map_err(|message| model_error(&message, &candidate))?;
        }
        candidate.prompt = trimmed.to_owned();
        candidate.cron = cron.map(|cron| cron.trim().to_owned());
        candidate.next_fire_at = candidate.next_fire_after(now).map_err(LoopError)?;
        let mut loops = state.loops.clone();
        loops.push(candidate.clone());
        persist(&state, &loops)?;
        state.loops = loops;
        Ok(candidate)
    }

    /// Reference `delete`.
    ///
    /// # Errors
    ///
    /// No loop has `loop_id`, or the save fails.
    pub fn delete(&self, loop_id: &str) -> Result<ScheduledPrompt, LoopError> {
        let mut state = self.lock()?;
        let found = state
            .loops
            .iter()
            .find(|item| item.id == loop_id)
            .cloned()
            .ok_or_else(|| LoopError(format!("No scheduled loop with id `{loop_id}`.")))?;
        let loops: Vec<ScheduledPrompt> = state
            .loops
            .iter()
            .filter(|item| item.id != loop_id)
            .cloned()
            .collect();
        persist(&state, &loops)?;
        state.loops = loops;
        Ok(found)
    }

    /// Reference `clear`: how many loops there were.
    ///
    /// # Errors
    ///
    /// The save fails.
    pub fn clear(&self) -> Result<usize, LoopError> {
        let mut state = self.lock()?;
        let count = state.loops.len();
        persist(&state, &[])?;
        state.loops.clear();
        Ok(count)
    }

    /// Seconds until the next loop is due, `None` with nothing scheduled.
    /// Reference `next_due_in`.
    #[must_use]
    pub fn next_due_in(&self, now: f64) -> Option<f64> {
        let state = self.state.lock().ok()?;
        state
            .loops
            .iter()
            .map(|item| item.next_fire_at)
            .min_by(f64::total_cmp)
            .map(|next| (next - now).max(0.0))
    }

    /// The loop due at `now`, the earliest first. Reference `due`.
    #[must_use]
    pub fn due(&self, now: f64) -> Option<ScheduledPrompt> {
        let state = self.state.lock().ok()?;
        state
            .loops
            .iter()
            .filter(|item| item.next_fire_at <= now)
            .min_by(|left, right| left.next_fire_at.total_cmp(&right.next_fire_at))
            .cloned()
    }

    /// Moves `loop_id` to its next run after `now`, saved when it can be and
    /// in memory otherwise. Reference `mark_fired` then `defer`.
    pub fn mark_fired(&self, loop_id: &str, now: f64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let loops: Vec<ScheduledPrompt> = state
            .loops
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if item.id == loop_id
                    && let Ok(next) = item.next_fire_after(now)
                {
                    item.next_fire_at = next;
                }
                item
            })
            .collect();
        let _ = persist(&state, &loops);
        state.loops = loops;
    }

    /// Runs one `cron` call. Reference `run_cron` over the session's
    /// `cron_scheduler`.
    pub fn run_tool(&self, arguments: &Value) -> ToolResult {
        if self
            .state
            .lock()
            .map(|state| state.disabled)
            .unwrap_or(true)
        {
            return ToolResult::failed(
                "tool_failed",
                "Scheduled prompts require a live interactive session",
            );
        }
        let text = |key: &str| {
            arguments
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let outcome = match text("action") {
            "schedule" => self
                .create_with(
                    arguments.get("interval_seconds").and_then(Value::as_u64),
                    None,
                    text("prompt"),
                )
                .map(|created| {
                    json!({
                        "verb": "Scheduled",
                        "loops": [created.dump()],
                        "message": format!("Scheduled loop {}", created.id),
                    })
                }),
            "schedule_cron" => self
                .create_with(None, Some(text("cron")), text("prompt"))
                .map(|created| {
                    json!({
                        "verb": "Scheduled",
                        "loops": [created.dump()],
                        "message": format!("Scheduled loop {}", created.id),
                    })
                }),
            "list" => {
                let loops = self.list();
                Ok(json!({
                    "verb": "Listed",
                    "loops": loops.iter().map(ScheduledPrompt::dump).collect::<Vec<_>>(),
                    "message": format!("Listed {} scheduled loops", loops.len()),
                }))
            }
            "cancel" => self.delete(text("id")).map(|deleted| {
                json!({
                    "verb": "Cancelled",
                    "loops": [deleted.dump()],
                    "message": format!("Cancelled loop {}", deleted.id),
                })
            }),
            "clear" => self.clear().map(|count| {
                json!({
                    "verb": "Cleared",
                    "loops": [],
                    "message": format!("Cleared {count} scheduled loops"),
                    "cleared_count": count,
                })
            }),
            other => Err(LoopError(format!("Unsupported cron action: {other}"))),
        };
        match outcome {
            Ok(payload) => ToolResult::Success {
                content: vec![
                    json!({"type": "text", "text": pycompat::dumps(&payload, Dumps::DEFAULT)}),
                ],
                structured_content: Some(payload),
                meta: None,
            },
            Err(LoopError(message)) => ToolResult::failed("tool_failed", message),
        }
    }
}

/// Writes `loops` where the schedule is saved, indented by two with a
/// final newline, nulls left out. Reference `_persist`.
fn persist(state: &Schedule, loops: &[ScheduledPrompt]) -> Result<(), LoopError> {
    let Some(path) = &state.path else {
        return Ok(());
    };
    write(path, loops).map_err(|error| {
        LoopError(format!(
            "Failed to persist scheduled loops at {}: {error}",
            path.display()
        ))
    })
}

fn write(path: &Path, loops: &[ScheduledPrompt]) -> std::io::Result<()> {
    let value = json!({
        "format": FORMAT,
        "loops": loops.iter().map(ScheduledPrompt::stored).collect::<Vec<_>>(),
    });
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomically(path, "loops", text.as_bytes(), FileMode::Private)
        .map_err(|error| error.source)
}

// --------------------------------------------------------------------------
// Cron expressions
// --------------------------------------------------------------------------

/// The values one field allows.
#[derive(Debug, Clone)]
struct Field {
    allowed: Vec<bool>,
    /// Whether the field is `*`, which matters for the day fields.
    any: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_value(text: &str, names: &[&str], offset: u32) -> Option<u32> {
    if let Ok(number) = text.parse::<u32>() {
        return Some(number);
    }
    let lowered = text.to_lowercase();
    names
        .iter()
        .position(|name| *name == lowered)
        .and_then(|index| u32::try_from(index).ok())
        .map(|index| index + offset)
}

fn parse_field(
    text: &str,
    low: u32,
    high: u32,
    names: &[&str],
    name_offset: u32,
) -> Result<Field, String> {
    let mut allowed = vec![false; usize::try_from(high + 1).unwrap_or_default()];
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("bad step `{step}`"))?,
            ),
            None => (item, 1),
        };
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((from, to)) = range.split_once('-') {
            let from = parse_value(from, names, name_offset)
                .ok_or_else(|| format!("bad value `{from}`"))?;
            let to =
                parse_value(to, names, name_offset).ok_or_else(|| format!("bad value `{to}`"))?;
            (from, to)
        } else {
            let value = parse_value(range, names, name_offset)
                .ok_or_else(|| format!("bad value `{range}`"))?;
            // `a/n` runs from `a` to the top of the range.
            (value, if item.contains('/') { high } else { value })
        };
        if start < low || end > high || start > end {
            return Err(format!("value out of range in `{item}`"));
        }
        let mut value = start;
        while value <= end {
            allowed[usize::try_from(value).unwrap_or_default()] = true;
            value += step;
        }
    }
    Ok(Field {
        allowed,
        any: text == "*",
    })
}

fn matches(field: &Field, value: i8) -> bool {
    usize::try_from(value)
        .ok()
        .and_then(|index| field.allowed.get(index))
        .copied()
        .unwrap_or(false)
}

/// The first run of `cron` strictly after `now`, in the machine's time
/// zone. Reference `_next_cron_fire` over croniter.
///
/// # Errors
///
/// An expression without five fields, or one croniter refuses.
pub fn next_cron_fire(cron: &str, now: f64) -> Result<f64, String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err("Cron must be a standard 5-field expression.".to_owned());
    }
    let invalid = |reason: String| format!("Invalid cron expression `{cron}`: {reason}");
    let minute = parse_field(fields[0], 0, 59, &[], 0).map_err(invalid)?;
    let hour = parse_field(fields[1], 0, 23, &[], 0).map_err(invalid)?;
    let day = parse_field(fields[2], 1, 31, &[], 0).map_err(invalid)?;
    let month = parse_field(fields[3], 1, 12, &MONTHS, 1).map_err(invalid)?;
    let mut weekday = parse_field(fields[4], 0, 7, &WEEKDAYS, 0).map_err(invalid)?;
    // Sunday is both 0 and 7.
    if weekday.allowed[7] {
        weekday.allowed[0] = true;
    }
    let zone = jiff::tz::TimeZone::system();
    #[allow(clippy::cast_possible_truncation)]
    let start = jiff::Timestamp::from_second(now.floor() as i64)
        .map_err(|error| invalid(error.to_string()))?
        .to_zoned(zone.clone());
    let mut date = start.date();
    let last = date
        .checked_add(jiff::Span::new().years(MAX_YEARS_BETWEEN_MATCHES))
        .map_err(|error| invalid(error.to_string()))?;
    while date <= last {
        let month_ok = matches(&month, date.month());
        let day_ok = matches(&day, date.day());
        let weekday_ok = matches(&weekday, date.weekday().to_sunday_zero_offset());
        // croniter's `day_or`: either restricted day field may match.
        let day_match = match (day.any, weekday.any) {
            (true, true) => true,
            (false, true) => day_ok,
            (true, false) => weekday_ok,
            (false, false) => day_ok || weekday_ok,
        };
        if month_ok && day_match {
            for hour_value in 0..24_i8 {
                if !matches(&hour, hour_value) {
                    continue;
                }
                for minute_value in 0..60_i8 {
                    if !matches(&minute, minute_value) {
                        continue;
                    }
                    let civil = date.at(hour_value, minute_value, 0, 0);
                    let Ok(zoned) = civil.to_zoned(zone.clone()) else {
                        continue;
                    };
                    #[allow(clippy::cast_precision_loss)]
                    let seconds = zoned.timestamp().as_second() as f64;
                    if seconds > now {
                        return Ok(seconds);
                    }
                }
            }
        }
        date = date
            .tomorrow()
            .map_err(|error| invalid(error.to_string()))?;
    }
    Err(invalid("no match within 50 years".to_owned()))
}

#[cfg(test)]
mod loops_tests;
