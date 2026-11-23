use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::*;
use serde_json::json;
use std::time::Duration;

fn checkpoint(session: &str) -> Checkpoint {
    let mut checkpoint =
        Checkpoint::new(RunId::new_v4(), session, "test-loop", 1, json!({"step":0}));
    checkpoint
        .transcript
        .push(Message::user(format!("input for {session}")));
    checkpoint
}
async fn store(path: &std::path::Path) -> SqliteStateStore {
    SqliteStateStore::open(path, SqliteOptions::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn transcript_state_and_events_survive_close_and_reopen_together() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let database = store(&path).await;
    let mut snapshot = database.create(checkpoint("restart"), None).await.unwrap();
    let lease = database
        .claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    let invocation = ToolExecution {
        invocation_id: RunId::new_v4(),
        tool_id: "transfer".into(),
        descriptor: ToolDescriptor::new("transfer", "transfer", "Transfer", json!({})),
        call: ToolCall::new("provider-call", "transfer", json!({"amount":17})),
        state: ToolExecutionState::Started,
        via_gateway: false,
        batch_item: None,
        preparation: None,
    };
    snapshot.state = serde_json::to_value(&invocation).unwrap();
    snapshot.status = ExecutionStatus::Failed;
    snapshot.events.push(EventEnvelope {
        scope: EventScope::Journal,
        schema_version: 1,
        run_id: snapshot.run_id,
        session_id: snapshot.session_id.clone(),

        sequence: 1,
        timestamp_unix_ms: unix_time_ms(),
        event: AgentEvent::ToolStarted {
            tool_id: invocation.tool_id.clone(),
            call_id: invocation.call.id.clone(),
        },
    });
    let snapshot = database.commit(&lease, 0, snapshot).await.unwrap();
    database.release(&lease).await.unwrap();
    database.close().await.unwrap();
    let reopened = store(&path).await;
    let loaded = reopened.load(snapshot.run_id).await.unwrap();
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.status, ExecutionStatus::Failed);
    assert_eq!(loaded.transcript, snapshot.transcript);
    assert_eq!(
        serde_json::from_value::<ToolExecution>(loaded.state).unwrap(),
        invocation
    );
    assert_eq!(loaded.events.len(), 1);
    assert_eq!(
        reopened.latest("restart").await.unwrap().unwrap().run_id,
        snapshot.run_id
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn independent_connections_compete_for_one_executor_and_reject_stale_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("race.db");
    let a = store(&path).await;
    let b = store(&path).await;
    let snapshot = a.create(checkpoint("race"), None).await.unwrap();
    let (left, right) = tokio::join!(
        a.claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30)),
        b.claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30))
    );
    let lease = match (left, right) {
        (Ok(lease), Err(StateError::Busy)) | (Err(StateError::Busy), Ok(lease)) => lease,
        outcome => panic!("expected one winner, got {outcome:?}"),
    };
    let mut next = snapshot.clone();
    next.state = json!({"step":1});
    let committed = a.commit(&lease, 0, next).await.unwrap();
    assert!(matches!(
        b.commit(&lease, 0, snapshot).await,
        Err(StateError::Conflict)
    ));
    assert_eq!(b.load(committed.run_id).await.unwrap().revision, 1);
    a.release(&lease).await.unwrap();
    let new_lease = b
        .claim(committed.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    assert!(new_lease.generation > lease.generation);
    assert!(matches!(
        a.commit(&lease, 1, committed.clone()).await,
        Err(StateError::LeaseLost)
    ));
    assert!(matches!(
        a.release(&lease).await,
        Err(StateError::LeaseLost)
    ));
    b.commit(&new_lease, 1, committed).await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();
}

#[tokio::test]
async fn expired_executor_is_fenced_even_when_its_owner_id_is_reused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lease.db");
    let database = store(&path).await;
    let snapshot = database.create(checkpoint("lease"), None).await.unwrap();
    let owner = RunId::new_v4();
    let lease = database
        .claim(snapshot.run_id, owner, Duration::from_millis(10))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(matches!(
        database.renew(&lease, Duration::from_secs(10)).await,
        Err(StateError::LeaseLost)
    ));
    let replacement = database
        .claim(snapshot.run_id, owner, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(matches!(
        database.commit(&lease, 0, snapshot.clone()).await,
        Err(StateError::LeaseLost)
    ));
    assert_eq!(
        database
            .commit(&replacement, 0, snapshot)
            .await
            .unwrap()
            .revision,
        1
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn renewal_preserves_fencing_identity_and_accepts_earlier_expiry_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let database = store(&directory.path().join("renewal.db")).await;
    let snapshot = database.create(checkpoint("renewal"), None).await.unwrap();
    let original = database
        .claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(5))
        .await
        .unwrap();
    let renewed = database
        .renew(&original, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(renewed.run_id, original.run_id);
    assert_eq!(renewed.owner, original.owner);
    assert_eq!(renewed.generation, original.generation);
    assert!(renewed.expires_at_unix_ms > original.expires_at_unix_ms);
    // Expiry is informational: the store checks its authoritative clock and row.
    let stale_snapshot = ExecutionLease {
        expires_at_unix_ms: 0,
        ..original.clone()
    };
    let snapshot = database.commit(&stale_snapshot, 0, snapshot).await.unwrap();
    database
        .renew(&original, Duration::from_secs(30))
        .await
        .unwrap();
    database.release(&stale_snapshot).await.unwrap();
    let replacement = database
        .claim(snapshot.run_id, original.owner, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(replacement.generation > original.generation);
    assert!(matches!(
        database.renew(&renewed, Duration::from_secs(30)).await,
        Err(StateError::LeaseLost)
    ));
    assert!(matches!(
        database.release(&renewed).await,
        Err(StateError::LeaseLost)
    ));
    database
        .commit(&replacement, snapshot.revision, snapshot)
        .await
        .unwrap();
    database.release(&replacement).await.unwrap();
    database.close().await.unwrap();
}

#[tokio::test]
async fn rejected_update_rolls_back_history_and_cursor_and_new_run_keeps_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    let database = store(&path).await;
    let snapshot = database.create(checkpoint("atomic"), None).await.unwrap();
    let lease = database
        .claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    let mut invalid = snapshot.clone();
    invalid.transcript.clear();
    invalid.state = json!({"step":999});
    assert!(matches!(
        database.commit(&lease, 0, invalid).await,
        Err(StateError::Invalid(_))
    ));
    let retained = database.load(snapshot.run_id).await.unwrap();
    assert_eq!(retained.state, json!({"step":0}));
    assert_eq!(retained.transcript, snapshot.transcript);
    assert!(matches!(
        database
            .create(checkpoint("atomic"), Some(snapshot.run_id))
            .await,
        Err(StateError::Busy)
    ));
    let mut complete = snapshot.clone();
    complete.status = ExecutionStatus::Completed;
    let complete = database.commit(&lease, 0, complete).await.unwrap();
    assert!(matches!(
        database.commit(&lease, 1, complete).await,
        Err(StateError::Conflict)
    ));
    let mut missing_history = checkpoint("atomic");
    missing_history.transcript = vec![Message::user("new only")];
    assert!(matches!(
        database
            .create(missing_history, Some(snapshot.run_id))
            .await,
        Err(StateError::Invalid(_))
    ));
    let mut next = checkpoint("atomic");
    next.transcript = snapshot.transcript;
    next.transcript.push(Message::user("next question"));
    let next = database.create(next, Some(snapshot.run_id)).await.unwrap();
    assert_eq!(
        database.latest("atomic").await.unwrap().unwrap().run_id,
        next.run_id
    );
    database.close().await.unwrap();
}

#[tokio::test]
async fn storage_limit_failure_does_not_advance_revision_or_head() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("size.db");
    let database = SqliteStateStore::open(
        &path,
        SqliteOptions {
            max_checkpoint_bytes: 2048,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let snapshot = database.create(checkpoint("size"), None).await.unwrap();
    let lease = database
        .claim(snapshot.run_id, RunId::new_v4(), Duration::from_secs(30))
        .await
        .unwrap();
    let mut too_large = snapshot.clone();
    too_large.state = json!({"data":"x".repeat(4096)});
    assert!(matches!(
        database.commit(&lease, 0, too_large).await,
        Err(StateError::Invalid(_))
    ));
    assert_eq!(database.load(snapshot.run_id).await.unwrap().revision, 0);
    database.close().await.unwrap();
}

#[test]
fn interaction_responses_bind_identity_schema_expiry_and_explicit_approval() {
    let mut question = InteractionRequest::input(
        "Choose count",
        json!({"type":"integer","minimum":1,"maximum":5}),
    )
    .unwrap();
    let good = InteractionResponse {
        interaction_id: question.id,
        value: json!(3),
    };
    question.validate_response(&good, 100).unwrap();
    assert!(
        question
            .validate_response(
                &InteractionResponse {
                    interaction_id: RunId::new_v4(),
                    value: json!(3)
                },
                100
            )
            .is_err()
    );
    assert!(
        question
            .validate_response(
                &InteractionResponse {
                    interaction_id: question.id,
                    value: json!(9)
                },
                100
            )
            .is_err()
    );
    question.expires_at_unix_ms = Some(100);
    assert!(question.validate_response(&good, 100).is_err());
    let approval = InteractionRequest::approval(RunId::new_v4(), "Apply change?");
    assert!(
        approval
            .validate_response(
                &InteractionResponse {
                    interaction_id: approval.id,
                    value: json!("yes")
                },
                0
            )
            .is_err()
    );
    approval
        .validate_response(
            &InteractionResponse {
                interaction_id: approval.id,
                value: json!(true),
            },
            0,
        )
        .unwrap();
}

#[tokio::test]
async fn terminal_commit_closes_interactions_atomically_and_survives_republication() {
    for status in [
        ExecutionStatus::Failed,
        ExecutionStatus::Cancelled,
        ExecutionStatus::Completed,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = store(&path).await;
        let mut cp = database.create(checkpoint("closed"), None).await.unwrap();
        let lease = database
            .claim(cp.run_id, RunId::new_v4(), Duration::from_secs(30))
            .await
            .unwrap();
        let record = InteractionRecord {
            session_id: "closed".into(),
            run_id: cp.run_id,
            request: InteractionRequest::input("Continue?", json!({"type":"boolean"})).unwrap(),
            tool_call: None,
            response: None,
            response_provenance: None,
            created_at_unix_ms: unix_time_ms(),
            answered_at_unix_ms: None,
        };
        database.publish(&record).await.unwrap();
        assert_eq!(database.pending("closed").await.unwrap().len(), 1);
        cp.status = status;
        database.commit(&lease, cp.revision, cp).await.unwrap();
        // No separate close_run call: emulate process loss immediately after commit.
        database.release(&lease).await.unwrap();
        database.close().await.unwrap();
        let reopened = store(&path).await;
        assert!(reopened.pending("closed").await.unwrap().is_empty());
        reopened.publish(&record).await.unwrap();
        let mut late = record.clone();
        late.request =
            InteractionRequest::input("Late request", json!({"type":"boolean"})).unwrap();
        reopened.publish(&late).await.unwrap();
        assert!(reopened.pending("closed").await.unwrap().is_empty());
        assert_eq!(
            reopened.get("closed", record.request.id).await.unwrap(),
            record
        );
        assert!(
            reopened
                .answer(
                    "closed",
                    &InteractionResponse {
                        interaction_id: record.request.id,
                        value: json!(true)
                    }
                )
                .await
                .is_err()
        );
        // Closing one run never suppresses another run's requests in the session.
        let mut other = late;
        other.run_id = RunId::new_v4();
        other.request = InteractionRequest::input("Other run", json!({"type":"boolean"})).unwrap();
        reopened.publish(&other).await.unwrap();
        assert_eq!(reopened.pending("closed").await.unwrap(), vec![other]);
        reopened.close().await.unwrap();
    }
}
