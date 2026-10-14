//! The skills a unified session can read, and the check a `skill` call
//! passes before it runs.
//!
//! Reference `harness/core/src/core/features/skills/`: a call naming a skill
//! the session does not hold is an invalid call, answered with up to three
//! close names and the full list, so the model can retry with an exact one.

/// How many close names a refusal suggests.
const SUGGESTIONS: usize = 3;

/// Checks that `arguments` name one of `available`, in the order the session
/// holds them.
pub fn check_request(available: &[&str], arguments: &serde_json::Value) -> Result<(), String> {
    let requested = arguments
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "skill name must be a non-empty string".to_owned())?;
    if available.contains(&requested) {
        return Ok(());
    }
    let quoted = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let close = closest(requested, available);
    let suggestion = if close.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", quoted(&close))
    };
    let listing = if available.is_empty() {
        " No skills are available.".to_owned()
    } else {
        format!(" Available skills: {}.", quoted(available))
    };
    Err(format!(
        "Skill {requested:?} is not available.{suggestion} Use an exact name from the available \
         skills below. Connector names and unlisted names in skill instructions are not \
         skills.{listing}"
    ))
}

/// The names most like `requested`, best first, ties kept in session order.
fn closest<'a>(requested: &str, available: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(&str, f64)> = available
        .iter()
        .map(|name| (*name, similarity(requested, name)))
        .filter(|(_, score)| *score > 0.0)
        .collect();
    scored.sort_by(|left, right| right.1.total_cmp(&left.1));
    scored
        .into_iter()
        .take(SUGGESTIONS)
        .map(|(name, _)| name)
        .collect()
}

/// How alike two names read once only their letters and digits count, in
/// lowercase: 1 when equal, 0.9 when one holds the other, otherwise one
/// minus their edit distance over the longer length.
fn similarity(left: &str, right: &str) -> f64 {
    let simplify = |text: &str| -> Vec<u8> {
        text.bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase())
            .collect()
    };
    let (left, right) = (simplify(left), simplify(right));
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    if left == right {
        return 1.0;
    }
    if contains(&left, &right) || contains(&right, &left) {
        return 0.9;
    }
    let longest = left.len().max(right.len());
    #[allow(clippy::cast_precision_loss)]
    let ratio = edit_distance(&left, &right) as f64 / longest as f64;
    1.0 - ratio
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Levenshtein distance over bytes.
fn edit_distance(left: &[u8], right: &[u8]) -> usize {
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.iter().enumerate() {
        let mut corner = row[0];
        row[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(corner + usize::from(a != b));
            corner = above;
        }
    }
    row[right.len()]
}

#[cfg(test)]
mod skills_tests;
