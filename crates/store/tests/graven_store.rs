use graven::ExpectedVersion;
use overseer_store::{SessionCreated, SessionEvent, Store, TurnEnd, TurnEndReason, UserMessage};

fn created(cwd: &str) -> SessionEvent {
    SessionEvent::Created(SessionCreated {
        cwd: cwd.to_owned(),
        model: "m".to_owned(),
    })
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        text: text.to_owned(),
    })
}

fn ended(reason: TurnEndReason) -> SessionEvent {
    SessionEvent::TurnEnd(TurnEnd { reason })
}

async fn store_suite(store: Store) {
    let id = "0199f0a0-0000-7000-8000-000000000001";

    assert_eq!(store.events(id).await.expect("reads").len(), 0);
    assert_eq!(store.version(id).await.expect("reads"), 0);

    let appended = store
        .append(
            id,
            ExpectedVersion::NoStream,
            &[created("/tmp"), user("hi")],
        )
        .await
        .expect("appends");
    assert_eq!(appended.new_version, 2);

    let events = store.events(id).await.expect("reads");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, created("/tmp"));
    assert_eq!(events[1].event, user("hi"));

    let appended = store
        .append(id, ExpectedVersion::Exact(2), &[ended(TurnEndReason::Done)])
        .await
        .expect("appends");
    assert_eq!(appended.new_version, 3);

    let rejected = store
        .append(id, ExpectedVersion::Exact(2), &[user("stale")])
        .await;
    assert!(
        rejected.is_err(),
        "a stale expected version must be refused"
    );
    assert_eq!(store.version(id).await.expect("reads"), 3);

    assert_eq!(
        store.session_ids().await.expect("lists"),
        vec![id.to_owned()]
    );
}

#[tokio::test]
async fn sqlite_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db"))
        .await
        .expect("opens");
    store_suite(store).await;
}

#[tokio::test]
async fn memory_backend() {
    store_suite(Store::memory()).await;
}

#[tokio::test]
async fn the_global_log_is_ordered_and_resumable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db"))
        .await
        .expect("opens");
    let first = "aaaaaaaa-0000-7000-8000-000000000001";
    let second = "aaaaaaaa-0000-7000-8000-000000000002";
    store
        .append(first, ExpectedVersion::NoStream, &[created("/a")])
        .await
        .expect("appends");
    store
        .append(second, ExpectedVersion::NoStream, &[created("/b")])
        .await
        .expect("appends");

    let all = store.global(0, 100).await.expect("reads");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].stream_id, first);
    assert_eq!(all[1].stream_id, second);
    assert_eq!(all[1].global_position, all[0].global_position + 1);

    let tail = store
        .global(all[0].global_position, 100)
        .await
        .expect("reads");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].stream_id, second);
}

#[tokio::test]
async fn a_session_summary_folds_the_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db"))
        .await
        .expect("opens");
    let id = "bbbbbbbb-0000-7000-8000-000000000001";

    assert!(store.summary(id).await.expect("reads").is_none());

    store
        .append(id, ExpectedVersion::NoStream, &[created("/repo")])
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert_eq!(summary.cwd, "/repo");
    assert_eq!(summary.model, "m");
    assert_eq!(summary.turns, 0);
    assert!(!summary.open_turn);

    store
        .append(id, ExpectedVersion::Exact(1), &[user("hi")])
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert_eq!(summary.turns, 1);
    assert!(summary.open_turn);
    assert_eq!(summary.last_turn, None);

    store
        .append(
            id,
            ExpectedVersion::Exact(2),
            &[ended(TurnEndReason::Cancelled)],
        )
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert!(!summary.open_turn);
    assert_eq!(summary.last_turn, Some(TurnEndReason::Cancelled));
}

#[tokio::test]
async fn an_unknown_stream_is_empty_after_a_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.db");
    let id = "cccccccc-0000-7000-8000-000000000001";
    {
        let store = Store::open(&path).await.expect("opens");
        store
            .append(id, ExpectedVersion::NoStream, &[created("/tmp")])
            .await
            .expect("appends");
    }

    let store = Store::open(&path).await.expect("reopens");
    assert_eq!(store.version(id).await.expect("reads"), 1);
    assert_eq!(store.events("missing").await.expect("reads").len(), 0);
}
