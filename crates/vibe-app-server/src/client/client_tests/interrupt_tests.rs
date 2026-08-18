//! Interrupting a turn: what the driver may refuse, and what the canonical
//! reservation does with the refusal.

use super::*;

struct RejectingInterruptDriver;

impl TurnDriver for RejectingInterruptDriver {
    fn run<'a>(&'a self, _reservation: &'a TurnReservation) -> DriverFuture<'a> {
        Box::pin(async { Err(DriverError::Tool("not executed".to_owned())) })
    }

    fn interrupt(&self, _session_id: &str, _turn_id: &str) -> Result<(), DriverError> {
        Err(DriverError::Tool("interrupt rejected".to_owned()))
    }
}

#[tokio::test]
async fn driver_rejection_prevents_canonical_interrupt_commit() {
    let mut service = HeadlessService::new(RejectingInterruptDriver).expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("pending"))
        .await
        .expect("turn reserves");

    assert!(
        service
            .interrupt(&session_id, &reservation.turn_id)
            .is_err()
    );
    let session = service
        .client
        .server
        .session(&session_id)
        .expect("session remains readable");
    assert_eq!(
        session.active_turn.as_deref(),
        Some(reservation.turn_id.as_str())
    );

    service
        .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
        .expect("reservation settles");
}

#[tokio::test]
async fn canonical_interrupt_rejection_is_reported_as_driver_only() {
    let mut service = HeadlessService::new(EchoTurnDriver::new("unused")).expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("pending"))
        .await
        .expect("turn reserves");

    assert!(matches!(
        service.interrupt(&session_id, "wrong-turn"),
        Ok(InterruptOutcome::DriverOnly { .. })
    ));
    let session = service
        .client
        .server
        .session(&session_id)
        .expect("session remains readable");
    assert_eq!(
        session.active_turn.as_deref(),
        Some(reservation.turn_id.as_str())
    );

    service
        .fail_reserved(&reservation, "test cleanup", TurnErrorCode::InternalError)
        .expect("reservation settles");
}

#[tokio::test]
async fn complete_interrupt_releases_the_canonical_turn_reservation() {
    let mut service = HeadlessService::new(EchoTurnDriver::new("unused")).expect("service starts");
    let session_id = service.start_session(&options()).expect("session starts");
    let reservation = service
        .reserve_prompt(&session_id, &TurnRequest::text("pending"))
        .await
        .expect("turn reserves");

    assert!(matches!(
        service.interrupt(&session_id, &reservation.turn_id),
        Ok(InterruptOutcome::Complete)
    ));
    let session = service
        .client
        .server
        .session(&session_id)
        .expect("session remains readable");
    assert_eq!(session.status, SessionStatus::Cancelled);
    assert!(session.active_turn.is_none());
    let state = service
        .public_call("session/read", json!({"sessionId": session_id}))
        .expect("public state remains readable");
    assert_eq!(
        state["state"]
            .pointer("/latestTurn/status")
            .and_then(Value::as_str),
        Some("interrupted")
    );
}
