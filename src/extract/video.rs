use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use super::run_tool;
use crate::db::models::VideoMeta;

const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn extract(path: &Path) -> anyhow::Result<VideoMeta> {
    let out = run_tool(
        "ffprobe",
        [
            "-v".as_ref(),
            "error".as_ref(),
            "-print_format".as_ref(),
            "json".as_ref(),
            "-show_format".as_ref(),
            "-show_streams".as_ref(),
            path.as_os_str(),
        ],
        PROBE_TIMEOUT,
    )
    .await?;
    parse_ffprobe(&out)
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
}

#[derive(Deserialize)]
struct Format {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

pub fn parse_ffprobe(json: &[u8]) -> anyhow::Result<VideoMeta> {
    let probe: Probe = serde_json::from_slice(json).context("parsing ffprobe output")?;
    let video = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));
    let format = probe.format.as_ref();

    let duration_secs = format
        .and_then(|f| f.duration.as_deref())
        .or_else(|| video.and_then(|v| v.duration.as_deref()))
        .and_then(|d| d.parse().ok());
    let fps = video.and_then(|v| {
        v.avg_frame_rate
            .as_deref()
            .and_then(parse_rate)
            .or_else(|| v.r_frame_rate.as_deref().and_then(parse_rate))
    });

    Ok(VideoMeta {
        duration_secs,
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        video_codec: video.and_then(|v| v.codec_name.clone()),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        fps,
        container: format.and_then(|f| f.format_name.clone()),
        bitrate: format
            .and_then(|f| f.bit_rate.as_deref())
            .and_then(|b| b.parse().ok()),
    })
}

/// Parses ffprobe rates like `30000/1001`; `0/0` yields `None`.
fn parse_rate(rate: &str) -> Option<f64> {
    let (n, d) = rate.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0 && n != 0.0).then(|| n / d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_json() {
        let json = br#"{
          "streams": [
            {"codec_type": "audio", "codec_name": "aac"},
            {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
             "avg_frame_rate": "30000/1001", "r_frame_rate": "30000/1001"}
          ],
          "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "12.500000", "bit_rate": "4000000"}
        }"#;
        let m = parse_ffprobe(json).unwrap();
        assert_eq!(m.video_codec.as_deref(), Some("h264"));
        assert_eq!(m.audio_codec.as_deref(), Some("aac"));
        assert_eq!((m.width, m.height), (Some(1920), Some(1080)));
        assert_eq!(m.duration_secs, Some(12.5));
        assert!((m.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(m.bitrate, Some(4_000_000));
    }

    #[test]
    fn rate_zero_is_none() {
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("25/1"), Some(25.0));
    }
}
