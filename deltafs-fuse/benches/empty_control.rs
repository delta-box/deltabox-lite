use deltafs_fuse::engine::{EngineOptions, StateEngine};
use std::fs;
use std::time::{Duration, Instant};

const DIFF_SAMPLES: usize = 201;
const CONTROL_SAMPLES: usize = 51;

fn measure(mut operation: impl FnMut()) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(DIFF_SAMPLES);
    for _ in 0..DIFF_SAMPLES {
        let start = Instant::now();
        operation();
        samples.push(start.elapsed());
    }
    samples
}

fn measure_n(count: usize, mut operation: impl FnMut()) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        operation();
        samples.push(start.elapsed());
    }
    samples
}

fn percentile(samples: &mut [Duration], percentile: usize) -> u128 {
    samples.sort_unstable();
    let index = (samples.len() - 1) * percentile / 100;
    samples[index].as_micros()
}

fn report(entries: usize, operation: &str, mut samples: Vec<Duration>) {
    let p50 = percentile(&mut samples, 50);
    let p95 = percentile(&mut samples, 95);
    println!("{entries},{operation},{p50},{p95}");
}

fn main() {
    println!("entries,operation,p50_us,p95_us");
    for entries in [10, 1_000, 10_000] {
        let root = tempfile::tempdir().expect("temporary benchmark root");
        let live = root.path().join("live");
        let state = root.path().join("state");
        fs::create_dir(&live).expect("LIVE directory");
        for index in 0..entries {
            fs::write(live.join(format!("file-{index:05}")), b"x").expect("fixture file");
        }

        let mut engine =
            StateEngine::create(&live, &state, EngineOptions::default()).expect("create engine");
        engine.diff(None).expect("warm diff");
        report(
            entries,
            "diff",
            measure(|| assert!(engine.diff(None).expect("empty diff").is_empty())),
        );
        report(
            entries,
            "checkpoint",
            measure_n(CONTROL_SAMPLES, || {
                engine.checkpoint(None).expect("empty checkpoint");
            }),
        );
        report(
            entries,
            "commit",
            measure_n(CONTROL_SAMPLES, || {
                engine.commit(None).expect("empty commit");
            }),
        );
    }
}
