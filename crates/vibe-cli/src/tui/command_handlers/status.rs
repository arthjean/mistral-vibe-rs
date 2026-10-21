//! `/status` and `/whoami`: the session's accounting and the caller it runs as.

use vibe_core::pycompat::thousands;

use super::{CommandBackend, Effect, ProviderAuth, Stats};

/// Reference `_show_status`.
pub(super) fn status_document(stats: &Stats) -> String {
    let cached = |tokens: u64| {
        if tokens > 0 {
            format!(" _(including {} cached)_", thousands(tokens))
        } else {
            String::new()
        }
    };
    format!(
        "## Agent Statistics\n\n- **Steps**: {}\n- **Session Prompt Tokens**: {}{}\n- **Session \
         Completion Tokens**: {}\n- **Session Total LLM Tokens**: {}\n- **Last Turn Tokens**: \
         {}{}\n- **Cost**: ${:.4}\n",
        thousands(stats.steps),
        thousands(stats.session_prompt_tokens),
        cached(stats.session_cached_tokens),
        thousands(stats.session_completion_tokens),
        thousands(stats.session_total_llm_tokens),
        thousands(stats.last_turn_total_tokens),
        cached(stats.last_turn_cached_tokens),
        stats.session_cost,
    )
}

/// Reference `_show_status`: the statistics, then the provider section when the
/// session runs the unified mode. The section is read-only and optional, so a
/// failed read leaves the statistics alone rather than adding an error.
pub(super) async fn show<B: CommandBackend>(backend: &mut B) {
    let mut document = status_document(&backend.stats());
    if backend.experimental_harness() {
        match backend.provider_auth().await {
            Ok(view) => {
                document.push('\n');
                document.push_str(&provider_auth_section(&view));
            }
            // The reference logs the failure's type alone, never its text,
            // which could carry what the redaction was meant to keep out.
            Err(_) => vibe_core::observability::log(
                vibe_core::observability::LogLevel::Warning,
                "Provider auth status read failed",
            ),
        }
    }
    backend.emit(Effect::Message(document));
}

/// Reference `render_provider_auth_section`
/// (`vibe/cli/textual_ui/provider_auth_status.py`): the "Model & Provider"
/// section, every dynamic value escaped so the Markdown it renders through
/// shows it literally.
fn provider_auth_section(view: &ProviderAuth) -> String {
    let api_base = view
        .api_base
        .as_deref()
        .map_or_else(|| "Invalid API URL".to_owned(), markdown_literal);
    format!(
        "## Model & Provider\n\n- **Model**: {}\n- **Provider**: {}\n- **API base**: {api_base}",
        markdown_literal(&view.model_display_name),
        markdown_literal(&view.provider_name),
    )
}

/// A one-line value as Markdown that renders it character for character.
///
/// A control character (Unicode category `Cc`, which is what
/// [`char::is_control`] tests) has no literal form on one line, so it becomes a
/// space. Every character that starts inline syntax, `.` and `:` included
/// because linkification reads them, is backslash-escaped; a tilde only next to
/// another tilde, since strikethrough needs a pair and a lone one is literal.
fn markdown_literal(value: &str) -> String {
    const INLINE: &str = "\\`*_.:<>&!#[]";
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<Vec<_>>();
    let mut literal = String::with_capacity(value.len());
    for (index, &character) in cleaned.iter().enumerate() {
        let paired_tilde = character == '~'
            && (index.checked_sub(1).and_then(|before| cleaned.get(before)) == Some(&'~')
                || cleaned.get(index + 1) == Some(&'~'));
        if paired_tilde || INLINE.contains(character) {
            literal.push('\\');
        }
        literal.push(character);
    }
    literal
}

/// Reference `_show_whoami`: a failed read is the same answer as no identity.
pub(super) async fn whoami<B: CommandBackend>(backend: &mut B) {
    let identity = backend.identity().await.ok().flatten();
    let plan = backend.account_plan().await.ok().flatten();
    let Some(identity) = identity else {
        backend.emit(Effect::Message(
            "## Who am I\n\nNo identity information is available for the active model.".to_owned(),
        ));
        return;
    };
    let mut lines = vec!["## Who am I".to_owned(), String::new()];
    if let Some(name) = identity
        .name
        .as_deref()
        .filter(|name| !name.is_empty() && Some(*name) != identity.email.as_deref())
    {
        lines.push(format!("- **Name**: {name}"));
    }
    if let Some(email) = identity.email.as_deref().filter(|email| !email.is_empty()) {
        lines.push(format!("- **Email**: {email}"));
    }
    if let Some(workspace) = &identity.workspace {
        lines.push(format!("- **Workspace**: {workspace}"));
    }
    if let Some(organization) = &identity.organization {
        lines.push(format!("- **Organization**: {organization}"));
    }
    if let Some(plan) = plan.filter(|plan| !plan.is_empty()) {
        lines.push(format!("- **Plan**: {plan}"));
    }
    backend.emit(Effect::Message(lines.join("\n")));
}
