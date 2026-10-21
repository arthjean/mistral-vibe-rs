use super::*;

fn host(running: Option<Running>) -> UnifiedHost {
    let meta = Map::new();
    UnifiedHost {
        store: None,
        marks: marks_of(&meta),
        meta,
        pins: SessionPins::default(),
        translator: Translator::new(
            Value::Null,
            0,
            AgentStatsSnapshot::priced(0.0, 0.0, None),
            0,
        ),
        interrupt: None,
        running,
        children: vibe_harness::subagents::ChildDirectory::default(),
        queue: queue::TurnQueue::default(),
        rewind: vibe_harness::rewind::RewindSnapshots::default(),
    }
}

fn conflict_message(fault: ProtocolFault) -> String {
    match fault {
        ProtocolFault::Other {
            code: ProtocolErrorCode::Conflict,
            message,
        } => message,
        _ => unreachable!("not a plain conflict"),
    }
}

#[test]
fn a_compaction_is_no_turn_a_client_can_address() {
    let compacting = host(Some(Running::Compaction));
    for expected in ["", "turn-1"] {
        let fault = compacting
            .require_turn(expected, "No matching active turn")
            .expect_err("a compaction runs no turn");
        assert_eq!(conflict_message(fault), "No active turn");
    }
}

#[test]
fn the_running_turn_answers_only_to_its_own_identifier() {
    let running = host(Some(Running::Turn("turn-1".to_owned())));
    assert!(running.require_turn("turn-1", "stale").is_ok());
    assert!(matches!(
        running.require_turn("", "stale"),
        Err(ProtocolFault::WithData {
            code: ProtocolErrorCode::StaleTurn,
            ..
        })
    ));
    assert!(host(None).require_turn("", "stale").is_err());
}

#[test]
fn a_compaction_keeps_the_session_busy() {
    assert!(host(Some(Running::Compaction)).require_idle().is_err());
    assert!(host(None).require_idle().is_ok());
}
