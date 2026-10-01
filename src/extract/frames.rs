use std::path::{Path, PathBuf};
use std::time::Duration;

use super::run_tool;

/// Seconds between sampled frames: the configured interval, widened for long videos
/// so that `max_frames` still covers the whole duration.
pub fn effective_interval(duration_secs: Option<f64>, interval_secs: u32, max_frames: u32) -> f64 {
    let base = f64::from(interval_secs.max(1));
    match duration_secs {
        Some(d) if d.is_finite() && d > 0.0 && max_frames > 0 => {
            base.max(d / f64::from(max_frames))
        }
        _ => base,
    }
}

/// Extracts the first frame, one frame every `interval` seconds, and the last frame
/// into `out_dir`, longest edge capped at `max_edge`. Returns `(timestamp_secs, path)`
/// in order. At most `max_frames` frames are returned.
pub async fn sample(
    video: &Path,
    out_dir: &Path,
    interval: f64,
    duration_secs: Option<f64>,
    max_frames: u32,
    max_edge: u32,
) -> anyhow::Result<Vec<(f64, PathBuf)>> {
    let scale = format!(
        "scale='if(gt(iw,ih),min({max_edge},iw),-2)':'if(gt(iw,ih),-2,min({max_edge},ih))'"
    );
    // `select` keeps the first frame and then the first frame at least `interval` seconds
    // after the previous pick. Unlike `fps=1/N` it does not drop a sample that falls within
    // N/2 seconds of the end of the video.
    let filter =
        format!("select='isnan(prev_selected_t)+gte(t-prev_selected_t,{interval:.3})',{scale}");
    let duration = duration_secs.filter(|d| d.is_finite() && *d > 0.0);
    // Keep one slot free for the last frame.
    let periodic_max = if duration.is_some() {
        max_frames.saturating_sub(1).max(1)
    } else {
        max_frames.max(1)
    };
    let pattern = out_dir.join("f_%05d.jpg");
    let frames = periodic_max.to_string();
    // Generous timeout: decoding long, high-resolution videos on slow disks takes a while.
    run_tool(
        "ffmpeg",
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-nostdin".as_ref(),
            "-i".as_ref(),
            video.as_os_str(),
            "-vf".as_ref(),
            filter.as_ref(),
            "-fps_mode".as_ref(),
            "vfr".as_ref(),
            "-frames:v".as_ref(),
            frames.as_ref(),
            "-q:v".as_ref(),
            "3".as_ref(),
            pattern.as_os_str(),
        ],
        Duration::from_secs(1800),
    )
    .await?;

    let mut files: Vec<PathBuf> = std::fs::read_dir(out_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("f_") && n.ends_with(".jpg"))
        })
        .collect();
    files.sort();
    let mut out: Vec<(f64, PathBuf)> = files
        .into_iter()
        .enumerate()
        .map(|(i, p)| (i as f64 * interval, p))
        .collect();

    if let Some(d) = duration {
        let last_ts = out.last().map_or(0.0, |(ts, _)| *ts);
        if needs_last_frame(d, last_ts, out.is_empty()) {
            match last_frame(video, out_dir, &scale).await {
                Ok(path) => out.push((d, path)),
                Err(e) => {
                    tracing::warn!(video = %video.display(), error = format!("{e:#}"), "could not extract last frame")
                }
            }
        }
    }
    Ok(out)
}

/// Minimum distance between the last periodic sample and the end of the video for the
/// last frame to be added (avoids near-duplicates such as samples at 60 s and 61 s).
const LAST_FRAME_MIN_GAP_SECS: f64 = 2.0;

fn needs_last_frame(duration: f64, last_sample_ts: f64, no_samples: bool) -> bool {
    no_samples || duration - last_sample_ts >= LAST_FRAME_MIN_GAP_SECS
}

/// Decodes the last seconds of the video and keeps only the final frame.
async fn last_frame(video: &Path, out_dir: &Path, scale: &str) -> anyhow::Result<PathBuf> {
    let path = out_dir.join("last.jpg");
    run_tool(
        "ffmpeg",
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-nostdin".as_ref(),
            "-sseof".as_ref(),
            "-3".as_ref(),
            "-i".as_ref(),
            video.as_os_str(),
            "-vf".as_ref(),
            scale.as_ref(),
            "-update".as_ref(),
            "1".as_ref(),
            "-q:v".as_ref(),
            "3".as_ref(),
            path.as_os_str(),
        ],
        Duration::from_secs(300),
    )
    .await?;
    anyhow::ensure!(path.exists(), "ffmpeg produced no last frame");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_widens_for_long_videos() {
        assert_eq!(effective_interval(Some(60.0), 10, 120), 10.0);
        assert_eq!(effective_interval(Some(3600.0), 10, 120), 30.0);
        assert_eq!(effective_interval(None, 10, 120), 10.0);
    }

    #[test]
    fn last_frame_only_when_not_a_near_duplicate() {
        assert!(needs_last_frame(32.0, 0.0, false));
        assert!(needs_last_frame(5.0, 0.0, false));
        assert!(!needs_last_frame(61.0, 60.0, false));
        assert!(needs_last_frame(70.0, 60.0, false));
        assert!(!needs_last_frame(1.0, 0.0, false));
        assert!(needs_last_frame(0.5, 0.0, true));
    }
}
