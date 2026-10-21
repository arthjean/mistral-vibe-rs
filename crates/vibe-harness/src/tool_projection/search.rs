//! Searches: `web_search`, `web_fetch` and `grep`.

use serde_json::{Value, json};
use vibe_core::pycompat;

use super::{
    Projected, call_display, completed_state, detail, nonempty, plural, result_display, structured,
};

pub(super) fn project_web_search(name: &str, input: &Value) -> Option<Projected> {
    let query = nonempty(input, "query")?.to_owned();
    let quoted = pycompat::repr_str(&query);
    let detail = detail(
        "web_search",
        name,
        json!({"query": query}),
        call_display(
            format!("Searching the web: {quoted}"),
            "Searching",
            format!("the web: {quoted}"),
            "Searched",
            format!("the web: {quoted}"),
            "Searching the web",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let query = result.get("query")?.as_str()?;
            let answer = result.get("answer").map_or(Some(""), Value::as_str)?;
            let mut sources = Vec::new();
            for source in result
                .get("sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                sources.push(json!({
                    "title": source.get("title")?.as_str()?,
                    "url": source.get("url")?.as_str()?,
                }));
            }
            let count = sources.len();
            let display = result_display(
                true,
                "Searched",
                format!(
                    "{} ({count} source{})",
                    pycompat::repr_str(query),
                    if count == 1 { "" } else { "s" }
                ),
            );
            let output = json!({"query": query, "answer": answer, "sources": sources});
            Some(completed_state(state, output, display, None))
        })),
    })
}

fn url_domain(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest);
    let netloc = after_scheme
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    if netloc.is_empty() {
        url.chars().take(50).collect()
    } else {
        netloc.to_owned()
    }
}

pub(super) fn project_web_fetch(name: &str, input: &Value) -> Option<Projected> {
    let url = nonempty(input, "url")?.to_owned();
    let timeout = match input.get("timeout") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64()?),
    };
    let mut message = url_domain(&url);
    if let Some(timeout) = timeout {
        message.push_str(&format!(" (timeout {timeout}s)"));
    }
    let detail = detail(
        "web_fetch",
        name,
        json!({"url": url, "timeout": timeout}),
        call_display(
            format!("Fetching: {message}"),
            "Fetching",
            message.as_str(),
            "Fetched",
            message.as_str(),
            "Fetching URL",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(|state| {
            let result = structured(state)?;
            let url = result.get("url")?.as_str()?;
            let content = result.get("content").map_or(Some(""), Value::as_str)?;
            let content_type = result.get("content_type").map_or(Some(""), Value::as_str)?;
            let truncated = result
                .get("was_truncated")
                .map_or(Some(false), Value::as_bool)?;
            let mut display = result_display(
                true,
                "Fetched",
                format!(
                    "{url} ({} chars, {})",
                    pycompat::thousands(u64::try_from(content.chars().count()).unwrap_or(u64::MAX)),
                    content_type.split(';').next().unwrap_or_default()
                ),
            );
            if truncated {
                display["suffix"] = json!("(truncated)");
            }
            let output = json!({
                "url": url,
                "content": content,
                "contentType": content_type,
                "wasTruncated": truncated,
            });
            Some(completed_state(state, output, display, None))
        })),
    })
}

pub(super) fn project_grep(name: &str, input: &Value) -> Option<Projected> {
    let pattern = nonempty(input, "pattern")?.to_owned();
    let path = input
        .get("path")
        .map_or(Some("."), Value::as_str)?
        .to_owned();
    let max_matches = match input.get("max_matches") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_i64()?),
    };
    let default_ignore = input
        .get("use_default_ignore")
        .map_or(Some(true), Value::as_bool)?;
    let mut message = format!("'{pattern}'");
    if path != "." {
        message.push_str(&format!(" in {path}"));
    }
    if let Some(max) = max_matches {
        message.push_str(&format!(" (max {max} matches)"));
    }
    if !default_ignore {
        message.push_str(" [no-ignore]");
    }
    let detail = detail(
        "file_search",
        name,
        json!({"pattern": pattern, "path": path, "maxMatches": max_matches}),
        call_display(
            format!("Grepping {message}"),
            "Searching",
            message.as_str(),
            "Searched",
            message.as_str(),
            "Searching files",
        ),
    );
    Some(Projected {
        detail,
        result: Some(Box::new(move |state| {
            let result = structured(state)?;
            let count = result.get("match_count")?.as_u64()?;
            let truncated = result
                .get("was_truncated")
                .map_or(Some(false), Value::as_bool)?;
            let mut display = result_display(
                true,
                "Searched",
                format!("{pattern} ({})", plural(count, "match", "matches")),
            );
            if truncated {
                display["suffix"] = json!("(truncated)");
            }
            let output = json!({
                "matches": result.get("matches").and_then(Value::as_str).unwrap_or_default(),
                "matchCount": count,
                "wasTruncated": truncated,
                "parsedMatches": result.get("parsed_matches").cloned().unwrap_or_else(|| json!([])),
            });
            Some(completed_state(state, output, display, None))
        })),
    })
}
