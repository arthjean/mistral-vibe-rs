//! The wire surface: the routed inventory, the handshake, the notifications
//! a client can mute, and the shapes every answer and every rejection carries.

use super::*;

#[test]
fn every_routed_method_is_declared_or_a_local_extension() {
    let routed = routed_methods();
    let undeclared = routed
        .iter()
        .filter(|method| !is_dispatchable_method(method))
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(
        undeclared,
        Vec::<&str>::new(),
        "routed but neither declared by the reference nor a local extension"
    );

    let advertised = advertised_methods();
    for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
        assert!(
            routed.contains(method),
            "{method} is a local extension but is routed nowhere"
        );
        assert!(
            !advertised.contains(&method.to_owned()),
            "{method} is a local extension and must not be advertised"
        );
    }
    assert!(advertised.iter().all(|method| is_server_method(method)));
    assert_eq!(
        advertised.len(),
        routed.len() - vibe_protocol::LOCAL_EXTENSION_METHODS.len(),
        "the advertised set is the routed methods minus the local extensions"
    );
}

/// A client library written against the reference protocol always may send
/// `disabledNotifications`. Rejecting it made the port unreachable for every
/// such client, since no second frame is ever sent after a failed
/// `initialize`.
#[test]
fn the_handshake_accepts_the_reference_capability_set() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    let response = initialize_with(
        &mut connection,
        json!({
            "callbackKinds": ["approval"],
            "clientTools": ["filesystem/read"],
            "disabledNotifications": ["runtime/updated"]
        }),
    );
    assert_eq!(connection.state(), ConnectionState::Ready);

    let advertised = response["capabilities"]["methods"]
        .as_array()
        .expect("the handshake advertises a method list")
        .iter()
        .filter_map(|method| method.as_str())
        .collect::<BTreeSet<_>>();
    for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
        assert!(
            !advertised.contains(method),
            "{method} is a local extension and must stay unadvertised"
        );
    }
    assert!(
        advertised.iter().all(|method| is_server_method(method)),
        "the handshake advertises a name the reference does not declare"
    );

    // A capability the reference does not declare still fails, which is
    // what keeps `deny_unknown_fields` discriminating the envelope.
    let mut fresh = server.connect(TransportKind::InProcess);
    let rejected = fresh.dispatch(&request(
        1,
        "initialize",
        json!({
            "clientInfo": {"name": "test", "version": "1"},
            "capabilities": {"invented": true}
        }),
    ));
    assert!(matches!(
        decode_frame(&rejected.outbound[0]).expect("rejection"),
        Envelope::Error(ErrorResponse {
            error: ProtocolError {
                code: ProtocolErrorCode::InvalidParams,
                ..
            },
            ..
        })
    ));
}

/// The mute list silences a notification the client does not want, and stops
/// at the sequenced event stream: dropping one of those would open a gap the
/// client's own projection reads as a fault.
#[test]
fn a_muted_notification_is_dropped_and_a_sequenced_event_is_not() {
    let workspace = tempfile::tempdir().expect("workspace");
    let open_session = |connection: &mut ServerConnection| {
        let started = connection.dispatch(&request(
            2,
            "session/start",
            json!({"sessionId": "session-1", "workingDirectory": workspace.path()}),
        ));
        // The answer, then the snapshot the attachment publishes.
        assert_eq!(started.outbound.len(), 2);
    };
    let trust = |connection: &mut ServerConnection| {
        connection.dispatch(&request(
            3,
            "workspace/trust/decision",
            json!({
                "sessionId": "session-1",
                "cwd": workspace.path(),
                "decision": "trust_cwd"
            }),
        ))
    };

    let server = AppServer::default();
    let mut listening = server.connect(TransportKind::InProcess);
    initialize_with(&mut listening, json!({"callbackKinds": ["approval"]}));
    open_session(&mut listening);
    assert_eq!(
        trust(&mut listening).outbound.len(),
        2,
        "an unmuted client receives the response and the notification"
    );

    let server = AppServer::default();
    let mut muted = server.connect(TransportKind::InProcess);
    initialize_with(
        &mut muted,
        json!({
            "callbackKinds": ["approval"],
            "disabledNotifications": ["runtime/updated"]
        }),
    );
    open_session(&mut muted);
    let batch = trust(&mut muted);
    assert_eq!(
        batch.outbound.len(),
        1,
        "a muted client receives the response alone"
    );
    assert!(matches!(
        decode_frame(&batch.outbound[0]).expect("trust answer"),
        Envelope::Success(_)
    ));

    // The mute consumed no event id, so the sequence still runs on from the
    // snapshot the attachment published.
    muted.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let started = server
        .turn_started("session-1", "turn-1")
        .expect("the turn starts");
    assert!(matches!(
        decode_frame(&started[0]).expect("started notification"),
        Envelope::Notification(Notification { ref params, .. })
            if params["eventId"] == json!(2)
    ));
    assert!(
        muted.delivers(&started[0]),
        "a sequenced event is delivered even when its name is muted"
    );
}

/// The status a `session/updated` publishes is the one a client renders,
/// so each transition has to name what it is waiting on or what broke.
#[test]
fn session_updated_names_the_turn_the_callback_and_the_failure() {
    let patch_status = |frame: &[u8]| -> Value {
        match decode_frame(frame).expect("status notification") {
            Envelope::Notification(Notification { method, params, .. }) => {
                assert_eq!(method, "session/updated");
                assert_eq!(params["sessionId"], json!("session-1"));
                assert!(params["emittedAt"].is_u64(), "the status is timestamped");
                assert_eq!(params["patch"][1]["path"], json!("/updatedAt"));
                assert_eq!(params["patch"][0]["path"], json!("/status"));
                params["patch"][0]["value"].clone()
            }
            other => unreachable!("expected a status notification: {other:?}"),
        }
    };

    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let started = server
        .turn_started("session-1", "turn-1")
        .expect("the turn starts");
    assert_eq!(
        patch_status(&started[1]),
        json!({"type": "running", "activeTurnId": "turn-1"})
    );

    let (callback_id, delivery) = connection
        .request_callback(
            "session-1",
            "turn-1",
            EngineCallbackKind::Approval,
            "May I run this?",
        )
        .expect("the callback is delivered");
    assert_eq!(
        patch_status(&delivery[0]),
        json!({
            "type": "blocked",
            "activeTurnId": "turn-1",
            "callbackId": callback_id,
            "reason": "approval",
        })
    );

    // A client attaching now is handed the state and the question still
    // open on it, so it can answer a callback raised before it arrived.
    let mut arriving = server.connect(TransportKind::InProcess);
    initialize_with(&mut arriving, json!({"callbackKinds": ["approval"]}));
    let attachment = arriving.attachment_frames("session-1");
    assert!(matches!(
        decode_frame(&attachment[0]).expect("snapshot"),
        Envelope::Notification(Notification { ref method, ref params, .. })
            if method == "session/snapshot"
                && params["state"]["eventId"] == params["eventId"]
    ));
    assert!(matches!(
        decode_frame(&attachment[1]).expect("redelivered callback"),
        Envelope::Request(ServerRequest { ref method, ref params, .. })
            if method == "callback/call"
                && params["callback"]["callbackId"] == json!(callback_id)
    ));

    let failed = server
        .fail_turn(
            "session-1",
            "turn-1",
            "the provider refused",
            TurnErrorCode::Refusal,
        )
        .expect("the turn fails");
    assert_eq!(
        patch_status(&failed[1]),
        json!({"type": "failed", "message": "the provider refused"})
    );
}

/// Usage reported mid-turn is pushed as it arrives and lands on the session
/// a client reads, so context pressure is visible before the turn settles.
#[test]
fn stats_updated_carries_the_whole_snapshot_and_the_session_token_usage() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": "hello"}]}),
    ));
    server
        .turn_started("session-1", "turn-1")
        .expect("the turn starts");
    let frame = server
        .record_turn_stats("session-1", "turn-1", 1_200, 900, 300)
        .expect("the usage is recorded");
    let Envelope::Notification(Notification { method, params, .. }) =
        decode_frame(&frame).expect("stats notification")
    else {
        unreachable!("the usage is published as a notification");
    };
    assert_eq!(method, "session/statsUpdated");
    assert_eq!(params["sessionId"], json!("session-1"));
    assert!(params["eventId"].as_u64().is_some_and(|id| id > 0));
    assert!(params["emittedAt"].is_u64());
    assert!(params["contextWindow"].is_u64(), "a threshold is published");
    assert_eq!(
        params["stats"]
            .as_object()
            .expect("the snapshot is an object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "cachedInputPricePerMillion",
            "contextTokens",
            "inputPricePerMillion",
            "lastTurnCachedTokens",
            "lastTurnCompletionTokens",
            "lastTurnDuration",
            "lastTurnPromptTokens",
            "outputPricePerMillion",
            "sessionCachedTokens",
            "sessionCompletionTokens",
            "sessionPromptTokens",
            "steps",
            "tokensPerSecond",
            "toolCallsAgreed",
            "toolCallsFailed",
            "toolCallsRejected",
            "toolCallsSucceeded",
        ],
        "the reference declares seventeen fields"
    );
    assert_eq!(params["stats"]["contextTokens"], json!(1_200));
    assert_eq!(params["stats"]["sessionPromptTokens"], json!(900));
    // A session with no completed turn reports zeroes rather than absences.
    assert_eq!(params["stats"]["lastTurnDuration"], json!(0.0));

    let read = connection.dispatch(&request(
        4,
        "session/read",
        json!({"sessionId": "session-1"}),
    ));
    let Envelope::Success(SuccessResponse { result, .. }) =
        decode_frame(&read.outbound[0]).expect("session state")
    else {
        unreachable!("session/read answers");
    };
    assert_eq!(
        result["state"]["session"]["tokenUsage"],
        json!({"inputTokens": 900, "outputTokens": 300, "totalTokens": 1_200})
    );
}

/// US-168: a `SKILL.md` that will not parse is a message on the surface an
/// operator reads, not a skill that silently failed to appear. The session
/// start is what projects it, and a second session adds nothing.
#[test]
fn an_unloadable_skill_is_reported_on_diagnostics_list() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let workspace = temporary.path().join("workspace");
    let broken = workspace.join(".vibe/skills/broken");
    std::fs::create_dir_all(&broken).expect("skill directory");
    std::fs::write(broken.join("SKILL.md"), "no frontmatter here\n").expect("skill fixture");
    let release3 = Release3Service::new(
        crate::release3::Release3Paths {
            vibe_home: temporary.path().join("home"),
            working_directory: workspace.clone(),
            session_root: temporary.path().join("sessions"),
        },
        true,
    )
    .expect("service");
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);

    for (id, session) in [(2, "session-1"), (3, "session-2")] {
        connection.dispatch(&request(
            id,
            "session/start",
            json!({"sessionId": session, "workingDirectory": workspace}),
        ));
    }
    let listed = connection.dispatch(&request(
        4,
        "diagnostics/list",
        json!({"sessionId": "session-1"}),
    ));
    let Envelope::Success(SuccessResponse { result, .. }) =
        decode_frame(&listed.outbound[0]).expect("diagnostics answer")
    else {
        unreachable!("diagnostics/list answers");
    };

    let issues = result["issues"].as_array().expect("an issues array");
    let skill_issues = issues
        .iter()
        .filter(|issue| {
            issue["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("broken/SKILL.md"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        skill_issues.len(),
        1,
        "one issue names the file, once across both sessions: {issues:?}"
    );
    assert!(
        skill_issues[0]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("Failed to load:")),
        "{skill_issues:?}"
    );
}

/// US-090: the single call a client renders a session from reports what the
/// session is actually running, not a fixed payload.
#[test]
fn runtime_read_reports_the_live_catalogs_configuration_and_accounting() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let runtime = call(&mut connection, 10, "runtime/read")["runtime"].clone();

    // The catalogs are the ones the dedicated calls publish, rather than
    // the empty lists this method used to answer with.
    let agents = call(&mut connection, 11, "agents/list");
    let skills = call(&mut connection, 12, "skills/list");
    assert_eq!(runtime["agents"], agents["agents"]);
    assert_eq!(runtime["skills"], skills["skills"]);
    assert_eq!(runtime["activeAgent"], agents["active"]);
    assert!(
        runtime["agents"]
            .as_array()
            .is_some_and(|agents| !agents.is_empty()),
        "the shipped profiles are published: {runtime}"
    );
    assert_eq!(
        runtime["tools"],
        call(&mut connection, 13, "tools/list")["tools"]
    );

    // The accounting and the threshold are the session's own, and the same
    // pair `stats/read` answers with.
    let stats = call(&mut connection, 14, "stats/read");
    assert_eq!(runtime["stats"], stats["stats"]);
    assert_eq!(runtime["contextWindow"], stats["contextWindow"]);
    assert_eq!(
        runtime["stats"]
            .as_object()
            .expect("the snapshot is an object")
            .len(),
        17,
        "the live snapshot is the one the notification carries"
    );

    // The configuration is a real view rather than an empty document, and
    // the hook count is counted rather than hard-coded.
    assert_eq!(runtime["config"], runtime["baseConfig"]);
    assert!(
        runtime["config"]["activeModel"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "the active model is named: {}",
        runtime["config"]
    );
    assert!(runtime["config"]["transcribeModels"].is_array());
    assert!(runtime["hooksCount"].is_u64());
    assert!(runtime["issues"].is_array());
}

/// US-093: the published session names the model and the agent it runs, so
/// a client renders them without a second call.
#[test]
fn the_published_session_names_its_model_and_agent() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    let batch = connection.dispatch(&request(
        2,
        "session/start",
        json!({
            "sessionId": "session-1",
            "workingDirectory": "/workspace",
            "model": "devstral-small",
            "agent": "plan"
        }),
    ));
    assert_eq!(batch.outbound.len(), 2);
    let session = call(&mut connection, 10, "session/read")["state"]["session"].clone();
    assert_eq!(session["model"], json!("devstral-small"));
    assert_eq!(
        session["agent"],
        json!({
            "name": "plan",
            "displayName": "Plan",
            "description": "Read-only agent for exploration and planning",
            "safety": "safe",
            "agentType": "agent",
        })
    );
    // The catalog names the same agent as the one the session runs, rather
    // than the one a fresh session would.
    assert_eq!(
        call(&mut connection, 11, "agents/list")["active"],
        session["agent"]
    );
}

/// US-148: the compaction policy is read once when a session opens and
/// carried on the session, beside the threshold the client renders.
#[test]
fn a_session_carries_the_compaction_policy_its_configuration_declares() {
    let temporary = tempfile::tempdir().expect("temporary home");
    let home = temporary.path().join("home");
    std::fs::create_dir_all(&home).expect("home directory");
    std::fs::write(
        home.join("config.toml"),
        concat!(
            "auto_compact_threshold = 30000\n",
            "compaction_prompt_id = \"terse\"\n",
            "context_warnings = true\n",
            "raise_on_compaction_failure = true\n",
            "active_model = \"tuned\"\n",
            "[[models]]\n",
            "name = \"tuned-model\"\n",
            "provider = \"mistral\"\n",
            "alias = \"tuned\"\n",
        ),
    )
    .expect("configuration fixture");
    let release3 = Release3Service::new(
        crate::release3::Release3Paths {
            vibe_home: home,
            working_directory: temporary.path().join("workspace"),
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("release-3 service");
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let session = server.session("session-1").expect("the session is open");
    assert_eq!(session.compaction.auto_compact_threshold, 30_000);
    assert_eq!(
        server.release3.context_window(),
        30_000,
        "the published threshold and the policy read the same key"
    );
    assert_eq!(
        session.compaction.compaction_model.as_deref(),
        Some("tuned-model")
    );
    assert_eq!(session.compaction.compaction_prompt_id, "terse");
    assert!(session.compaction.context_warnings);
    assert!(session.compaction.raise_on_compaction_failure);
}

/// US-091: the configuration answers carry the two views and the runtime the
/// reference declares, and nothing else.
///
/// The server runs against a temporary home: the patch below writes a file,
/// and a test must never write the operator's own configuration.
#[test]
fn the_configuration_envelopes_are_the_reference_shapes() {
    let temporary = tempfile::tempdir().expect("temporary home");
    let release3 = Release3Service::new(
        crate::release3::Release3Paths {
            vibe_home: temporary.path().join("home"),
            working_directory: temporary.path().join("workspace"),
            session_root: temporary.path().join("sessions"),
        },
        false,
    )
    .expect("release-3 service");
    let server = AppServer::with_release3_service(release3);
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);

    let read = call(&mut connection, 10, "config/read");
    assert_eq!(
        read.keys().map(String::as_str).collect::<Vec<_>>(),
        ["baseConfig", "config", "strippedHistoryImages"]
    );
    assert_eq!(read["config"], read["baseConfig"]);
    assert_eq!(
        read["config"]
            .as_object()
            .expect("the view is an object")
            .len(),
        18,
        "the view is the whole `ConfigView`"
    );
    assert!(read["strippedHistoryImages"].is_u64());

    // A reload publishes the runtime rather than the views, which is what
    // separates the two shapes upstream.
    let reload = call(&mut connection, 11, "config/reload");
    assert_eq!(
        reload.keys().map(String::as_str).collect::<Vec<_>>(),
        ["runtime", "strippedHistoryImages"]
    );
    assert!(reload["runtime"]["config"]["activeModel"].is_object());

    let patched = connection.dispatch(&request(
        12,
        "config/patch",
        json!({
            "sessionId": "session-1",
            "ops": [{"op": "set", "path": "/theme", "value": "nord"}],
        }),
    ));
    let Envelope::Success(SuccessResponse { result, .. }) =
        decode_frame(&patched.outbound[0]).expect("patch answer")
    else {
        unreachable!("config/patch answers");
    };
    assert_eq!(
        result.keys().map(String::as_str).collect::<Vec<_>>(),
        ["failures", "rejected", "runtime", "strippedHistoryImages"]
    );
    assert_eq!(result["rejected"], json!(false));
    assert_eq!(result["failures"], json!([]));
    assert_eq!(result["runtime"]["config"]["theme"], json!("nord"));
}

/// US-090: the session's logging state, rather than a fixed disabled
/// summary, across the six fields the wire declares.
#[test]
fn runtime_read_reports_the_session_log_summary() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let answer = call(&mut connection, 10, "runtime/read");
    let log = &answer["sessionLog"];
    assert_eq!(
        log.as_object()
            .expect("the summary is an object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "enabled",
            "needsInitialAutoTitle",
            "path",
            "persisted",
            "sessionId",
            "title",
        ]
    );
    // The default configuration writes sessions, so the switch is reported
    // as on even though this in-memory session is not persisted.
    assert_eq!(log["enabled"], json!(true));
    assert_eq!(log["persisted"], json!(false));
    assert_eq!(log["sessionId"], Value::Null);
    assert_eq!(answer["ready"], json!(true));
}

/// US-090: the account is classified from the credential the session runs
/// under, so a configured key is never reported as missing.
#[test]
fn account_read_classifies_the_configured_credential() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let account = call(&mut connection, 10, "account/read")["account"].clone();
    let status = account["status"].as_str().expect("a status is published");
    assert!(
        ["ready", "missing_key", "unauthorized", "unavailable"].contains(&status),
        "{status} is outside the account vocabulary"
    );
    // The default configuration serves a Mistral model, so the answer turns
    // on whether a key resolves rather than being fixed. The classification
    // reads the environment and then the OS keyring, as the reference's
    // `resolve_api_key` does, so the expectation mirrors both sources.
    let ambient = std::env::var("MISTRAL_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .or_else(|| vibe_core::auth::KeyringStore::native().get_api_key("MISTRAL_API_KEY"));
    let expected = if ambient.is_some_and(|key| !key.is_empty()) {
        "ready"
    } else {
        "missing_key"
    };
    assert_eq!(status, expected);
    assert_eq!(account["teleportAction"]["kind"], json!("upgrade_to_pro"));
}

/// The local extensions keep answering the clients already calling them,
/// while staying outside the advertised contract.
#[test]
fn a_local_extension_stays_routable() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    for method in vibe_protocol::LOCAL_EXTENSION_METHODS {
        let batch = connection.dispatch(&request(9, method, json!({"sessionId": "session-1"})));
        let frame = decode_frame(&batch.outbound[0]).expect("extension answer");
        if let Envelope::Error(ErrorResponse { error, .. }) = frame {
            assert_ne!(
                error.code,
                ProtocolErrorCode::MethodNotFound,
                "{method} is routed nowhere: {}",
                error.message
            );
        }
    }
}

/// A client that sent the wrong shape has to be told which value was wrong,
/// not merely that something was.
#[test]
fn invalid_params_names_the_offending_value() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    let batch = connection.dispatch(&request(
        3,
        "turn/start",
        json!({"sessionId": "session-1", "input": [{"type": "text", "text": 7}]}),
    ));
    let Envelope::Error(ErrorResponse { error, .. }) =
        decode_frame(&batch.outbound[0]).expect("rejection")
    else {
        unreachable!("a malformed turn input was accepted");
    };
    assert_eq!(error.code, ProtocolErrorCode::InvalidParams);
    assert_eq!(error.data["errorCount"], json!(1));
    let issue = &error.data["issues"][0];
    // The path is field names and array indices, not a flattened string.
    // It stops at the content block rather than reaching `/text`: the block
    // is an untagged variant, and serde reports the failure where it gave
    // up on the variant, not inside the one it never selected.
    assert_eq!(
        issue["path"],
        json!(["input", 0]),
        "the path names the field and index that failed: {}",
        error.data
    );
    assert!(
        issue["message"].as_str().is_some_and(|m| !m.is_empty()),
        "the issue carries a message"
    );

    // A rejection that is not a deserialization failure has no path to point
    // at, so `data` is null. The key is still on the wire: the reference dumps
    // its error payload without a null filter, so a detail-free error frame
    // there has the same three keys as one that carries a detail.
    let batch = connection.dispatch(&request(
        4,
        "turn/start",
        json!({"sessionId": "absent", "input": [{"type": "text", "text": "hello"}]}),
    ));
    let Envelope::Error(ErrorResponse { error, .. }) =
        decode_frame(&batch.outbound[0]).expect("rejection")
    else {
        unreachable!("an unknown session was accepted");
    };
    assert_ne!(error.code, ProtocolErrorCode::InvalidParams);
    let encoded = serde_json::to_value(&error).expect("error encodes");
    assert_eq!(
        encoded.get("data"),
        Some(&Value::Null),
        "a non-deserialization rejection carries a null data key: {encoded}"
    );
}

/// Most methods are answered by the resource, release3 and release4
/// dispatchers, which check their parameters by hand rather than through a
/// deserializer. A client reads the same structured detail from those as
/// from the handful this module parses itself.
#[test]
fn a_dispatcher_rejection_carries_the_same_structured_detail() {
    let server = AppServer::default();
    let mut connection = server.connect(TransportKind::InProcess);
    initialize(&mut connection);
    start_session(&mut connection);
    // One method per dispatcher family, each missing a required parameter.
    for (method, params) in [
        ("tools/list", json!({})),
        ("session/title/update", json!({"sessionId": "session-1"})),
        ("loops/delete", json!({"sessionId": "session-1"})),
    ] {
        let batch = connection.dispatch(&request(5, method, params));
        let Envelope::Error(ErrorResponse { error, .. }) =
            decode_frame(&batch.outbound[0]).expect("rejection")
        else {
            unreachable!("{method} accepted parameters it should have rejected");
        };
        assert_eq!(
            error.code,
            ProtocolErrorCode::InvalidParams,
            "{method}: {}",
            error.message
        );
        assert_eq!(error.data["errorCount"], json!(1), "{method}");
        let issue = &error.data["issues"][0];
        assert!(
            issue["path"].is_array(),
            "{method} carries no path: {}",
            error.data
        );
        assert!(
            issue["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{method} carries no message: {}",
            error.data
        );
    }
}

/// `vibe-core` and `vibe-protocol` sit in the same dependency layer, so the
/// callback vocabulary is spelled twice. Both spellings cross the wire, so
/// their JSON forms have to stay identical.
#[test]
fn callback_kinds_share_one_wire_form() {
    for (engine, wire) in [
        (EngineCallbackKind::Approval, CallbackKind::Approval),
        (EngineCallbackKind::UserInput, CallbackKind::UserInput),
        (
            EngineCallbackKind::ConnectorAuth,
            CallbackKind::ConnectorAuth,
        ),
    ] {
        assert_eq!(
            serde_json::to_value(engine).expect("engine kind"),
            serde_json::to_value(wire).expect("wire kind"),
            "{engine:?} and {wire:?} must serialize identically"
        );
    }
}
