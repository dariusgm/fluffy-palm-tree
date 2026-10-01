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

/// Extracts one JPEG every `interval` seconds into `out_dir`, longest edge capped at
/// `max_edge`. Returns `(timestamp_secs, path)` in order.
pub async fn sample(
    video: &Path,
    out_dir: &Path,
    interval: f64,
    max_frames: u32,
    max_edge: u32,
) -> anyhow::Result<Vec<(f64, PathBuf)>> {
    let filter = format!(
        "fps=1/{interval:.3},scale='if(gt(iw,ih),min({max_edge},iw),-2)':'if(gt(iw,ih),-2,min({max_edge},ih))'"
    );
    let pattern = out_dir.join("f_%05d.jpg");
    let frames = max_frames.to_string();
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
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect();
    files.sort();
    Ok(files
        .into_iter()
        .enumerate()
        .map(|(i, p)| (i as f64 * interval, p))
        .collect())
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
}
