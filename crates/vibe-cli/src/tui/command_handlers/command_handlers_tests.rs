//! The handlers' pure helpers. What each command emits is compared against the
//! reference by the `handlers` family in `commands_parity_tests.rs`.

use vibe_core::observability::{LogLevel, LogLevelChain};

use super::log_level::{Applied, Badge, LogLevelBackend, Picker, apply};
use super::mcp_arguments::{parse_add, split_posix};
use super::{
    Effect, ScheduledLoop, Stats, format_duration, format_loop_list, grouped, session_cost,
    short_session_id, status_document,
};

#[test]
fn posix_splitting_follows_python_shlex() {
    assert_eq!(
        split_posix(r#"a 'b c' "d \" e" f\ g "" h\\"#).unwrap(),
        ["a", "b c", "d \" e", "f g", "", "h\\"]
    );
    assert_eq!(split_posix(r#""a\b""#).unwrap(), ["a\\b"]);
    assert_eq!(split_posix("'open"), Err("No closing quotation"));
    assert_eq!(split_posix("trailing\\"), Err("No escaped character"));
}

#[test]
fn mcp_add_arguments_follow_the_reference_grammar() {
    let parsed = parse_add(
        "https://mcp.example/sse --name docs --scope read --scope write --transport http \
         --no-login --allow-insecure-http",
    )
    .unwrap();
    assert_eq!(parsed.url, "https://mcp.example/sse");
    assert_eq!(parsed.name.as_deref(), Some("docs"));
    assert_eq!(parsed.scopes, ["read", "write"]);
    assert_eq!(parsed.transport, "http");
    assert!(!parsed.login);
    assert!(parsed.allow_insecure_http);

    let defaults = parse_add("https://mcp.example").unwrap();
    assert_eq!(defaults.transport, "streamable-http");
    assert!(defaults.login);
    assert!(defaults.name.is_none());

    assert_eq!(
        parse_add("https://a --name x --name y").unwrap_err(),
        "Usage: /mcp add accepts --name only once."
    );
    assert_eq!(
        parse_add("https://a --name").unwrap_err(),
        "Usage: /mcp add --name <alias>"
    );
    assert_eq!(
        parse_add("https://a --transport sse").unwrap_err(),
        "MCP server transport must be one of: http, streamable-http."
    );
    assert_eq!(
        parse_add("https://a --bogus").unwrap_err(),
        "Unknown /mcp add option: --bogus"
    );
    assert_eq!(
        parse_add("'https://a").unwrap_err(),
        "Invalid /mcp add arguments: No closing quotation"
    );
}

#[test]
fn durations_list_every_unit_or_only_the_largest() {
    assert_eq!(format_duration(3661, false), "1h1m1s");
    assert_eq!(format_duration(3661, true), "1h");
    assert_eq!(format_duration(0, false), "0s");
    assert_eq!(format_duration(86_400 * 2 + 5, false), "2d5s");
}

#[test]
fn the_loop_table_escapes_cells_and_clamps_overdue_runs() {
    let loops = [
        ScheduledLoop {
            id: "loop-1".to_owned(),
            prompt: "check | deploy\nreport".to_owned(),
            interval_seconds: 3661,
            next_fire_at: 130.9,
        },
        ScheduledLoop {
            id: "loop-2".to_owned(),
            prompt: "late".to_owned(),
            interval_seconds: 60,
            next_fire_at: 50.0,
        },
    ];
    assert_eq!(
        format_loop_list(&loops, 100.0),
        "| Prompt | Next in | Every | ID |\n|--------|------|-------|----|\n| check \\| deploy \
         report | 30s | 1h1m1s | `loop-1` |\n| late | 0s | 1m | `loop-2` |"
    );
    assert_eq!(format_loop_list(&[], 100.0), "No scheduled loops.");
}

#[test]
fn status_groups_thousands_and_prices_cached_tokens() {
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(1_234_567), "1,234,567");
    // Cached tokens bill at the cached price, and never beyond the prompt.
    assert!((session_cost(1_000_000, 0, 400_000, 2.0, 6.0, Some(0.5)) - 1.4).abs() < 1e-9);
    assert!((session_cost(10, 1_000_000, 50, 2.0, 6.0, None) - 6.00002).abs() < 1e-9);
    let document = status_document(&Stats {
        steps: 3,
        session_prompt_tokens: 12_000,
        session_cached_tokens: 2_000,
        session_cost: 0.123_456,
        ..Stats::default()
    });
    assert!(document.contains("- **Session Prompt Tokens**: 12,000 _(including 2,000 cached)_"));
    assert!(document.contains("- **Last Turn Tokens**: 0\n"));
    assert!(document.ends_with("- **Cost**: $0.1235\n"));
    assert_eq!(short_session_id("0123456789abcdef"), "01234567");
}

#[derive(Default)]
struct Levels {
    session: Option<LogLevel>,
    env: Option<LogLevel>,
    config: Option<LogLevel>,
    persist_error: Option<String>,
    effects: Vec<Effect>,
}

impl LogLevelBackend for Levels {
    fn chain(&self) -> LogLevelChain {
        LogLevelChain::resolve(self.session, self.env, self.config)
    }

    fn set_session_override(&mut self, level: Option<LogLevel>) {
        self.session = level;
    }

    fn persist(&mut self, level: Option<LogLevel>) -> Result<(), String> {
        self.persist_error.clone().map_or(Ok(()), Err)?;
        self.config = level;
        Ok(())
    }

    fn emit(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
}

#[test]
fn the_picker_moves_badges_and_reports_the_effective_level() {
    let mut picker = Picker::new(LogLevelChain::resolve(None, None, Some(LogLevel::Info)));
    assert_eq!(picker.highlighted(), LogLevel::Info);
    assert_eq!(picker.subtitle(), "Effective: INFO  (config.toml: INFO)");
    picker.highlight(LogLevel::Debug);
    picker.toggle();
    assert_eq!(picker.session(), Some(LogLevel::Debug));
    assert_eq!(
        picker.subtitle(),
        "Effective: DEBUG  (session override: DEBUG)"
    );
    picker.focus(Badge::Config);
    picker.highlight(LogLevel::Info);
    picker.toggle();
    assert_eq!(picker.config(), None);
    assert_eq!(
        picker.applied(),
        Applied {
            session: Some(LogLevel::Debug),
            config: None,
            config_cleared: true,
        }
    );
}

#[test]
fn applying_the_picker_reports_each_change_or_the_failure() {
    let mut levels = Levels {
        config: Some(LogLevel::Info),
        ..Levels::default()
    };
    apply(
        Applied {
            session: Some(LogLevel::Debug),
            config: None,
            config_cleared: true,
        },
        &mut levels,
    );
    assert_eq!(
        levels.effects,
        [
            Effect::ClosePanel,
            Effect::Message(
                "session override → DEBUG  config.toml cleared  (effective: DEBUG)".to_owned()
            ),
        ]
    );

    let mut unchanged = Levels::default();
    apply(
        Applied {
            session: None,
            config: None,
            config_cleared: false,
        },
        &mut unchanged,
    );
    assert_eq!(
        unchanged.effects,
        [
            Effect::ClosePanel,
            Effect::Message("Log level unchanged  (effective: WARNING)".to_owned()),
        ]
    );

    let mut failing = Levels {
        persist_error: Some("read-only".to_owned()),
        ..Levels::default()
    };
    apply(
        Applied {
            session: None,
            config: Some(LogLevel::Error),
            config_cleared: false,
        },
        &mut failing,
    );
    assert_eq!(
        failing.effects,
        [
            Effect::ClosePanel,
            Effect::Error("Failed to persist log-level config: read-only".to_owned()),
        ]
    );
}
