//! Session deletion: the tombstone that holds the key while the runtime, the
//! task registry, the stored session and the task spool all go away.

use super::super::*;
use super::fixtures::*;
use coda_agent::ToolApprovalMode;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};

/// Attach as connection 1, using the defaults every test here wants.
async fn attach(hub: &SessionHub, conn: ConnId) -> Result<AttachSession, AttachError> {
    hub.attach(
        key(),
        conn,
        "prov".into(),
        None,
        PermissionMode::default(),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_evicts_attached_client_and_removes_entry() {
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let attached = attach(&hub, 1).await.expect("attach");
    let mut events = attached.events;

    assert!(matches!(hub.delete(key(), 1).await, DeleteOutcome::Deleted));
    next_matching(&mut events, |e| matches!(e, RelayEvent::Evicted)).await;
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_from_stale_connection_is_rejected() {
    // Latest-wins covers destruction too: after being evicted, the old
    // connection must not be able to delete the session the new client is
    // driving.
    let (hub, _) = hub_with("reply", ToolApprovalMode::Auto);
    let _attach1 = attach(&hub, 1).await.expect("attach");
    let _attach2 = hub
        .attach(
            key(),
            2,
            "prov".into(),
            None,
            PermissionMode::default(),
            true,
        )
        .await
        .expect("attach2 evicts conn 1");

    assert!(matches!(
        hub.delete(key(), 1).await,
        DeleteOutcome::NotOwner
    ));
    assert!(hub.get_entry(&key()).is_some());

    // The attached client itself may delete.
    assert!(matches!(hub.delete(key(), 2).await, DeleteOutcome::Deleted));
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn persisted_state_goes_only_after_the_runtime_and_registry_are_down() {
    // The spool is removed out from under the task monitors, so they have to be
    // finished with it first: a monitor still writing a manifest into a
    // directory being deleted is exactly what the ordering here rules out.
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attached = attach(&hub, 1).await.expect("attach");
    let background = background_of(&hub).await;
    let cancelled = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let task_cancelled = cancelled.clone();
    let task_finish = finish.clone();
    background
        .spawn_with(task_meta("delete barrier"), move |ctx| async move {
            ctx.cancelled().cancelled().await;
            task_cancelled.notify_one();
            task_finish.notified().await;
            coda_process::TaskExit::Killed
        })
        .await
        .unwrap();

    let delete_hub = hub.clone();
    let delete = tokio::spawn(async move { delete_hub.delete(key(), 1).await });
    timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("delete did not enter registry shutdown");
    assert!(
        timeout(Duration::from_millis(50), opener.delete_entered.notified())
            .await
            .is_err(),
        "persisted state was deleted while a task monitor was still running"
    );
    assert!(
        hub.get_entry(&key()).is_some(),
        "map entry removed before the delete finished"
    );

    finish.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(5), delete)
            .await
            .expect("delete did not finish")
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    assert_eq!(
        opener.deleted.lock().unwrap().as_slice(),
        &[key()],
        "the session's persisted state was never deleted"
    );
    assert!(hub.get_entry(&key()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_waits_for_the_delete_transaction() {
    // The window this closes: the hub used to free the key as soon as the
    // runtime was down, so an attach could open the session again while its
    // rows and its spool were still being removed underneath it.
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    let gate = Arc::new(Notify::new());
    opener.delete_gate = Some(gate.clone());
    let (hub, opener) = hub_and_opener(opener);
    let _attached = attach(&hub, 1).await.expect("attach");

    let delete_hub = hub.clone();
    let delete = tokio::spawn(async move { delete_hub.delete(key(), 1).await });
    timeout(Duration::from_secs(5), opener.delete_entered.notified())
        .await
        .expect("delete never reached the persisted state");

    let attach_hub = hub.clone();
    let mut attaching = tokio::spawn(async move { attach(&attach_hub, 2).await.is_ok() });
    assert!(
        timeout(Duration::from_millis(50), &mut attaching)
            .await
            .is_err(),
        "attach opened the session mid-delete"
    );
    assert!(
        hub.get_entry(&key()).is_some(),
        "the tombstone left the map before the delete finished"
    );

    gate.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(5), delete)
            .await
            .expect("delete did not finish")
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    assert!(
        timeout(Duration::from_secs(5), attaching)
            .await
            .expect("attach never woke")
            .unwrap(),
        "attach failed after the delete finished"
    );
    // Opening a session is where the real opener (re-)creates its stored row,
    // so this order is the invariant the row depends on: the racing open builds
    // its session strictly after the delete removed the old one's state, not
    // before it — which would leave a live session with nothing to write to.
    assert_eq!(
        opener.calls.lock().unwrap().as_slice(),
        &["open", "delete_persisted", "open"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_with_nothing_live_still_holds_the_key() {
    // Deleting from the catalog is the common case and there is nothing live to
    // lock, so the delete borrows a slot for its tombstone — otherwise a
    // concurrent open would race the rows on their way out.
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    let gate = Arc::new(Notify::new());
    opener.delete_gate = Some(gate.clone());
    let (hub, opener) = hub_and_opener(opener);
    assert!(hub.get_entry(&key()).is_none());

    let delete_hub = hub.clone();
    let delete = tokio::spawn(async move { delete_hub.delete(key(), 1).await });
    timeout(Duration::from_secs(5), opener.delete_entered.notified())
        .await
        .expect("a cold delete never reached the persisted state");
    assert!(
        hub.get_entry(&key()).is_some(),
        "a cold delete left the key unheld"
    );

    let attach_hub = hub.clone();
    let mut attaching = tokio::spawn(async move { attach(&attach_hub, 1).await.is_ok() });
    assert!(
        timeout(Duration::from_millis(50), &mut attaching)
            .await
            .is_err(),
        "attach opened a session the delete had not finished with"
    );

    gate.notify_one();
    assert!(matches!(
        timeout(Duration::from_secs(5), delete)
            .await
            .expect("delete did not finish")
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    assert!(
        timeout(Duration::from_secs(5), attaching)
            .await
            .expect("attach never woke")
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_delete_leaves_the_session_reachable() {
    // The rows are still there, so the key has to be given back: the client is
    // told the delete failed, and reopening the session must work.
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    opener.delete_error = Some("storage is down".into());
    let (hub, _) = hub_and_opener(opener);
    let _attached = attach(&hub, 1).await.expect("attach");

    let outcome = hub.delete(key(), 1).await;
    assert!(
        matches!(&outcome, DeleteOutcome::Failed(error) if error == "storage is down"),
        "a failed delete must not report success"
    );
    assert!(
        hub.get_entry(&key()).is_none(),
        "the tombstone outlived the failed delete"
    );
    attach(&hub, 1)
        .await
        .expect("reattach after a failed delete");
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_removes_its_task_spool() {
    let (hub, opener) = hub_and_opener(TestOpener::new("reply", ToolApprovalMode::Auto));
    let _attached = attach(&hub, 1).await.expect("attach");
    background_of(&hub)
        .await
        .spawn_with(task_meta("echo spooled"), |_ctx| async {
            coda_process::TaskExit::Exited { code: Some(0) }
        })
        .await
        .unwrap();
    let spool = spool_dir(&opener, &key());
    assert!(spool.exists(), "the task never spooled anything");

    assert!(matches!(hub.delete(key(), 1).await, DeleteOutcome::Deleted));
    assert!(!spool.exists(), "the deleted session kept its task spool");

    // A session reopened under the same id starts from an empty archive rather
    // than inventorying the tasks of the one that was deleted.
    let _reattached = attach(&hub, 1).await.expect("reattach");
    let tasks = background_of(&hub).await.summaries().borrow().clone();
    assert!(
        tasks.is_empty(),
        "a reopened session inherited the deleted one's tasks: {tasks:?}"
    );
    hub.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_delete_waiter_does_not_strand_the_key() {
    // The delete belongs to the hub, not to whoever asked for it: an aborted
    // request task used to take it down with it, leaving a tombstone nothing
    // could ever lift.
    let mut opener = TestOpener::new("reply", ToolApprovalMode::Auto);
    let gate = Arc::new(Notify::new());
    opener.delete_gate = Some(gate.clone());
    let (hub, opener) = hub_and_opener(opener);
    let _attached = attach(&hub, 1).await.expect("attach");
    let spool = spool_dir(&opener, &key());

    let delete_hub = hub.clone();
    let delete = tokio::spawn(async move { delete_hub.delete(key(), 1).await });
    timeout(Duration::from_secs(5), opener.delete_entered.notified())
        .await
        .expect("delete never reached the persisted state");
    delete.abort();
    assert!(delete.await.unwrap_err().is_cancelled());

    gate.notify_one();
    timeout(Duration::from_secs(5), async {
        while hub.get_entry(&key()).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the abandoned delete never dropped its tombstone");
    assert_eq!(
        opener.deleted.lock().unwrap().as_slice(),
        &[key()],
        "the abandoned delete never removed the persisted state"
    );
    assert!(!spool.exists(), "the abandoned delete kept the task spool");

    let attached = timeout(Duration::from_secs(5), attach(&hub, 2))
        .await
        .expect("attach never woke from the abandoned delete")
        .expect("reopen after an abandoned delete");
    drop(attached);
    hub.shutdown_all().await;
}
