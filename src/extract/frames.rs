use std::path::{Path, PathBuf};
use std::time::Duration;

use super::run_tool;

/// Seconds between two sampled frames at the very least; avoids near-duplicates.
const MIN_GAP_SECS: f64 = 2.0;

/// Timestamps to sample: first frame, middle and last frame. Videos of unknown or very
/// short duration only get the frames that are not near-duplicates of each other.
pub fn sample_times(duration_secs: Option<f64>) -> Vec<f64> {
    match duration_secs.filter(|d| d.is_finite() && *d > 0.0) {
        None => vec![0.0],
        Some(d) if d < MIN_GAP_SECS => vec![0.0],
        Some(d) if d < 2.0 * MIN_GAP_SECS => vec![0.0, d],
        Some(d) => vec![0.0, d / 2.0, d],
    }
}

/// Extracts the first frame, the one at about 50 % and the last frame into `out_dir`,
/// longest edge capped at `max_edge`. Returns `(timestamp_secs, path)` in order.
pub async fn sample(
    video: &Path,
    out_dir: &Path,
    duration_secs: Option<f64>,
    max_edge: u32,
) -> anyhow::Result<Vec<(f64, PathBuf)>> {
    let scale = format!(
        "scale='if(gt(iw,ih),min({max_edge},iw),-2)':'if(gt(iw,ih),-2,min({max_edge},ih))'"
    );
    let times = sample_times(duration_secs);
    let last_index = times.len() - 1;
    let mut out = Vec::with_capacity(times.len());
    for (i, ts) in times.iter().copied().enumerate() {
        let is_last = i == last_index && i > 0;
        let frame = if is_last {
            last_frame(video, out_dir, &scale).await
        } else {
            frame_at(video, out_dir, ts, i, &scale).await
        };
        match frame {
            Ok(path) => out.push((ts, path)),
            // The first frame is required; a failing middle or last frame is not fatal.
            Err(e) if i == 0 => return Err(e),
            Err(e) => {
                tracing::warn!(video = %video.display(), error = format!("{e:#}"), "could not extract frame at {ts:.0}s")
            }
        }
    }
    Ok(out)
}

/// Decodes a single frame at `ts` (seeking on the input, so long videos stay fast).
async fn frame_at(
    video: &Path,
    out_dir: &Path,
    ts: f64,
    index: usize,
    scale: &str,
) -> anyhow::Result<PathBuf> {
    let path = out_dir.join(format!("f_{index:05}.jpg"));
    let seek = format!("{ts:.3}");
    run_tool(
        "ffmpeg",
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-nostdin".as_ref(),
            "-ss".as_ref(),
            seek.as_ref(),
            "-i".as_ref(),
            video.as_os_str(),
            "-vf".as_ref(),
            scale.as_ref(),
            "-frames:v".as_ref(),
            "1".as_ref(),
            "-update".as_ref(),
            "1".as_ref(),
            "-q:v".as_ref(),
            "3".as_ref(),
            path.as_os_str(),
        ],
        Duration::from_secs(600),
    )
    .await?;
    anyhow::ensure!(path.exists(), "ffmpeg produced no frame at {ts:.0}s");
    Ok(path)
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
    fn first_middle_and_last_frame() {
        assert_eq!(sample_times(Some(60.0)), vec![0.0, 30.0, 60.0]);
        assert_eq!(sample_times(Some(25.0)), vec![0.0, 12.5, 25.0]);
        assert_eq!(sample_times(Some(4.0)), vec![0.0, 2.0, 4.0]);
    }

    #[test]
    fn short_or_unknown_videos_skip_near_duplicates() {
        assert_eq!(sample_times(Some(3.0)), vec![0.0, 3.0]);
        assert_eq!(sample_times(Some(1.0)), vec![0.0]);
        assert_eq!(sample_times(None), vec![0.0]);
        assert_eq!(sample_times(Some(f64::NAN)), vec![0.0]);
    }
}
