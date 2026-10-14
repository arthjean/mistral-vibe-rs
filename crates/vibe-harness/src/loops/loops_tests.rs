use super::*;

#[test]
fn intervals_read_as_the_reference_reads_them() {
    assert_eq!(parse_interval("5m"), Ok(300));
    assert_eq!(parse_interval(" 2H "), Ok(7200));
    assert!(
        parse_interval("10s")
            .unwrap_err()
            .0
            .contains("at least 30s")
    );
    assert!(
        parse_interval("5x")
            .unwrap_err()
            .0
            .starts_with("Invalid interval `5x`.")
    );
    assert_eq!(
        parse_interval(""),
        Err(LoopError("Missing interval.".to_owned()))
    );
}

#[test]
fn a_cron_expression_fires_on_its_next_matching_minute() {
    let now = jiff::civil::date(2026, 10, 13)
        .at(19, 55, 13, 0)
        .to_zoned(jiff::tz::TimeZone::system())
        .unwrap();
    #[allow(clippy::cast_precision_loss)]
    let seconds = now.timestamp().as_second() as f64;
    let next = next_cron_fire("0 9 * * 1", seconds).unwrap();
    #[allow(clippy::cast_possible_truncation)]
    let fired = jiff::Timestamp::from_second(next as i64)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::system());
    assert_eq!(fired.date(), jiff::civil::date(2026, 10, 19));
    assert_eq!((fired.hour(), fired.minute()), (9, 0));
    let every = next_cron_fire("*/15 * * * *", seconds).unwrap();
    assert!(every > seconds && every - seconds <= 15.0 * 60.0);
    assert!(
        next_cron_fire("bad", seconds)
            .unwrap_err()
            .contains("5-field")
    );
    assert!(
        next_cron_fire("61 * * * *", seconds)
            .unwrap_err()
            .starts_with("Invalid cron expression")
    );
}

#[test]
fn the_tool_schedules_lists_and_clears() {
    let loops = ScheduledLoops::new();
    let scheduled = loops
        .run_tool(&json!({"action": "schedule", "interval_seconds": 120, "prompt": " Check "}));
    let ToolResult::Success {
        structured_content: Some(payload),
        ..
    } = scheduled
    else {
        panic!("not scheduled");
    };
    assert_eq!(payload["loops"][0]["prompt"], "Check");
    assert_eq!(payload["loops"][0]["interval_seconds"], 120);
    assert!(payload["loops"][0].get("cron").is_none());
    let refused = loops.run_tool(&json!({"action": "schedule_cron", "cron": "bad", "prompt": "x"}));
    let ToolResult::Failure { message, .. } = refused else {
        panic!("accepted a bad expression");
    };
    assert!(
        message.starts_with("1 validation error for ScheduledPrompt\n  Value error, Cron must be")
    );
    assert!(message.ends_with("input_type=dict]"));
    let cleared = loops.run_tool(&json!({"action": "clear"}));
    let ToolResult::Success {
        structured_content: Some(payload),
        ..
    } = cleared
    else {
        panic!("not cleared");
    };
    assert_eq!(payload["cleared_count"], 1);
}

#[test]
fn the_schedule_is_saved_without_nulls() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scheduled-loops.json");
    let loops = ScheduledLoops::new();
    loops.create("5m", "Build").unwrap();
    loops.attach(path.clone()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with("}\n"));
    assert!(!text.contains("null"));
    let restored = ScheduledLoops::new();
    restored.restore(path).unwrap();
    assert_eq!(restored.list(), loops.list());
}
