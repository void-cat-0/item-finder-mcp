use super::*;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_retains_blocking_permit_until_work_really_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("facts.db");
    drop(Store::open(&path).unwrap());
    let db = Database::new(path);
    let permits = db.permits.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task = tokio::spawn(async move {
        db.read_snapshot(
            Instant::now() + Duration::from_millis(250),
            Arc::new(AtomicBool::new(false)),
            move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            },
        )
        .await
    });
    entered_rx.await.unwrap();
    assert_eq!(permits.available_permits(), 3);
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["error"]["code"],
        "timeout"
    );
    assert_eq!(permits.available_permits(), 3);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while permits.available_permits() != 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn permit_wait_is_inside_absolute_request_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::new(dir.path().join("missing.db"));
    let all = db.permits.clone().acquire_many_owned(4).await.unwrap();
    let error = db
        .read_snapshot(
            Instant::now() + Duration::from_millis(20),
            Arc::new(AtomicBool::new(false)),
            |_| Ok(()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["error"]["code"],
        "timeout"
    );
    drop(all);
    assert_eq!(db.permits.available_permits(), 4);
}

#[tokio::test]
async fn cancellation_flag_stops_snapshot_before_operation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("facts.db");
    drop(Store::open(&path).unwrap());
    let db = Database::new(path);
    let flag = Arc::new(AtomicBool::new(true));
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let error = db
        .read_snapshot(Instant::now() + Duration::from_secs(2), flag, move |_| {
            observed.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["error"]["code"],
        "cancelled"
    );
    assert!(!called.load(Ordering::Relaxed));
}
