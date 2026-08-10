#![cfg(target_os = "macos")]

use deltafs_fuse::macos::{current_event_id, EventStream, ITEM_IS_HARDLINK};
use std::fs;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn stream_barrier_and_persisted_cursor_do_not_replay_completed_work() {
    let root = tempfile::Builder::new()
        .prefix("deltafs-events-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let start = current_event_id();
    let stream = EventStream::start(&[root.path().to_path_buf()], start).unwrap();
    stream.history(TIMEOUT).unwrap();

    fs::write(root.path().join("a.txt"), "one").unwrap();
    fs::write(root.path().join("a.txt"), "two").unwrap();
    fs::rename(root.path().join("a.txt"), root.path().join("b.txt")).unwrap();
    fs::hard_link(root.path().join("b.txt"), root.path().join("peer.txt")).unwrap();
    fs::remove_file(root.path().join("peer.txt")).unwrap();
    let barrier = root.path().join(".barrier");
    let batch = stream.barrier(&barrier, TIMEOUT).unwrap();
    assert!(batch
        .events
        .iter()
        .any(|event| event.path.ends_with("b.txt")));
    assert!(batch
        .events
        .iter()
        .any(|event| event.flags & ITEM_IS_HARDLINK != 0));

    drop(stream);
    let reopened = EventStream::start(&[root.path().to_path_buf()], batch.latest_id).unwrap();
    let history = reopened.history(TIMEOUT).unwrap();
    assert!(
        history.events.is_empty(),
        "completed events replayed from cursor {}: {:?}",
        batch.latest_id,
        history.events
    );
}
