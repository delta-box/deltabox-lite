use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Debug-build test hook: pause this process at a named point until a test
/// releases it, so concurrency tests can construct exact interleavings
/// instead of racing timers. Inert unless both env vars are set. A forgotten
/// release aborts after 30s so a broken test cannot hang the suite.
///
/// Protocol: when DELTABOX_TEST_PAUSE_AT matches `point`, touch
/// `$DELTABOX_TEST_PAUSE_DIR/<point>.arrived` and block until
/// `<point>.release` exists.
pub(crate) fn maybe_pause(point: &str) {
    if !cfg!(debug_assertions) {
        return;
    }
    if std::env::var("DELTABOX_TEST_PAUSE_AT").as_deref() != Ok(point) {
        return;
    }
    let Some(dir) = std::env::var_os("DELTABOX_TEST_PAUSE_DIR").map(PathBuf::from) else {
        return;
    };
    let _ = std::fs::write(dir.join(format!("{point}.arrived")), b"");
    let release = dir.join(format!("{point}.release"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        if Instant::now() > deadline {
            std::process::abort();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
