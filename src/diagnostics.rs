//! Records frame pacing and per-frame timings for `--profile-frames` diagnostics.
//!
//! Presentation pacing is measured only in forced-redraw profiling mode. Idle wakeups are not
//! frames, and CPU submission time is not a GPU timestamp.
use serde::Serialize;
use std::time::Instant;

/// Presentations ignored at the start, while caches and the driver warm up.
const WARMUP: usize = 10;

/// Accumulates swap-to-swap intervals and frame timings after [`WARMUP`].
#[derive(Default)]
pub struct Profile {
    presented: usize,
    previous: Option<Instant>,
    intervals: Vec<f64>,
    cpu: Vec<f64>,
    timeline: Vec<Frame>,
}
/// One profiled frame: where its time went (milliseconds) and how much of it was redrawn.
#[derive(Clone, Copy, Serialize)]
pub struct Frame {
    /// Time since the previous presentation; set by [`Profile::record`].
    pub interval_ms: f64,
    /// Building the scene's draw commands.
    pub compose_ms: f64,
    /// Issuing the GL commands, as CPU time.
    pub render_ms: f64,
    /// The buffer swap.
    pub swap_ms: f64,
    /// The redrawn share of the window, from 0 to 1.
    pub redrawn: f64,
    /// First-use work inside `render_ms`: tessellating new glyph outlines.
    pub glyph_ms: f64,
    /// First-use work inside `render_ms`: loading new logo meshes.
    pub vector_ms: f64,
    /// GPU uploads of images decoded since the previous frame.
    pub image_ms: f64,
}
/// Statistics over a series of millisecond samples.
#[derive(Serialize)]
pub struct Timing {
    pub samples: usize,
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}
/// The `profile` section of the diagnostics JSON.
#[derive(Serialize)]
pub struct Summary {
    pub presented: usize,
    pub warmup_frames: usize,
    pub pacing: Option<Timing>,
    pub cpu_submission: Option<Timing>,
    pub timeline: Vec<Frame>,
}
impl Profile {
    /// Records one presentation at `now`.
    ///
    /// The interval is kept only after [`WARMUP`] presentations and when `exclude` is false, as
    /// for a frame stalled by a screenshot capture; either way `now` becomes the next interval's
    /// start.
    pub fn record(&mut self, now: Instant, cpu_ms: f64, exclude: bool, mut frame: Frame) {
        self.presented += 1;
        if self.presented > WARMUP
            && !exclude
            && let Some(previous) = self.previous
        {
            frame.interval_ms = now.duration_since(previous).as_secs_f64() * 1000.;
            self.intervals.push(frame.interval_ms);
            self.cpu.push(cpu_ms);
            self.timeline.push(frame);
        }
        self.previous = Some(now);
    }
    /// Drops the pending interval start, e.g. while the window is hidden.
    pub fn pause(&mut self) {
        self.previous = None;
    }
    pub fn summary(&self) -> Summary {
        Summary {
            presented: self.presented,
            warmup_frames: WARMUP,
            pacing: summarize(&self.intervals),
            cpu_submission: summarize(&self.cpu),
            timeline: self.timeline.clone(),
        }
    }
}
/// Returns the mean, nearest-rank 95th percentile and maximum, or `None` without samples.
fn summarize(samples: &[f64]) -> Option<Timing> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(Timing {
        samples: sorted.len(),
        mean_ms: sorted.iter().sum::<f64>() / sorted.len() as f64,
        p95_ms: sorted[(sorted.len() * 95).div_ceil(100) - 1],
        max_ms: *sorted.last()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn profiling_excludes_warmup_and_capture_stalls() {
        let mut profile = Profile::default();
        let start = Instant::now();
        for index in 0..20 {
            profile.record(
                start + Duration::from_millis(index * 16),
                2.,
                index == 19,
                Frame {
                    interval_ms: 0.,
                    compose_ms: 0.,
                    render_ms: 2.,
                    swap_ms: 0.,
                    redrawn: 1.,
                    glyph_ms: 0.,
                    vector_ms: 0.,
                    image_ms: 0.,
                },
            );
        }
        let summary = profile.summary();
        let pacing = summary.pacing.unwrap();
        assert_eq!(pacing.samples, 9);
        assert_eq!(pacing.mean_ms, 16.);
        assert_eq!(pacing.p95_ms, 16.);
        assert_eq!(summary.cpu_submission.unwrap().mean_ms, 2.);
    }
}
