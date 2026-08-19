//! The waterfall's contrast bounds are estimated from a sample of the
//! spectrogram rather than all of it, because live capture recomputes them
//! every second against a buffer that reaches 31 MB. This checks the estimate
//! against the full computation it replaced — on real traffic, since the whole
//! question is whether the sample still catches the narrow signal peaks that
//! the top percentile is made of.

use ragchew::spectrogram::WINDOW;
use ragchew::Spectrogram;
use std::time::Instant;

/// The full computation, as it was before sampling.
fn exact(spec: &Spectrogram) -> (f32, f32) {
    let mut vals: Vec<f32> = Vec::new();
    for col in &spec.columns {
        for &m in col {
            vals.push((m + 1e-9).ln());
        }
    }
    if vals.is_empty() {
        return (0.0, 1.0);
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f32| vals[((p * (vals.len() as f32 - 1.0)) as usize).min(vals.len() - 1)];
    (pct(0.55), pct(0.995))
}

/// The sampled bounds must land where the exact ones do, and must cost a small
/// fraction of what they did. The exact version at this size took 3.3 s in a
/// debug build and ran every second: the app stopped answering.
#[test]
fn sampled_percentiles_agree_and_are_cheap() {
    let hop = WINDOW / 4;
    // Real traffic, not a synthetic ramp: the demo band is a whole 4 kHz of
    // stations plus noise, which is the distribution this has to summarize.
    let samples = ragchew_gui::demo::synth();
    let mut spec = Spectrogram::compute(&samples, 0.0, 4000.0, hop);
    // Grow it to the live cap by repeating the band.
    while spec.columns.len() < 12_000 {
        let more = Spectrogram::compute(&samples, 0.0, 4000.0, hop);
        spec.columns.extend(more.columns);
    }
    spec.columns.truncate(12_000);

    let t = Instant::now();
    let e = exact(&spec);
    let exact_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let s = ragchew_gui::waterfall::percentiles(&spec);
    let sampled_ms = t.elapsed().as_secs_f64() * 1000.0;

    println!("exact   {e:?}  {exact_ms:.1} ms");
    println!("sampled {s:?}  {sampled_ms:.1} ms   ({:.0}x faster)", exact_ms / sampled_ms.max(1e-6));

    let span = e.1 - e.0;
    assert!((s.0 - e.0).abs() < span * 0.01, "low percentile moved: {} vs {}", s.0, e.0);
    assert!((s.1 - e.1).abs() < span * 0.01, "high percentile moved: {} vs {}", s.1, e.1);
    assert!(sampled_ms < exact_ms / 10.0, "only {:.1}x faster", exact_ms / sampled_ms);
}
