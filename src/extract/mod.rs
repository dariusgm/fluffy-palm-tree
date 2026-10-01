pub mod document;
pub mod frames;
pub mod image;
pub mod prepare;
pub mod video;

use std::ffi::OsStr;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::process::Command;

/// Runs an external tool without a shell and returns stdout.
pub async fn run_tool<I, S>(program: &str, args: I, timeout: Duration) -> anyhow::Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, child)
        .await
        .with_context(|| format!("{program} timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("running {program} (is it installed? see INSTALLATION.md)"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{program} exited with {}: {}",
            out.status,
            stderr.trim().chars().take(500).collect::<String>()
        );
    }
    Ok(out.stdout)
}

/// True if `program` can be executed (used by tests to skip when tools are missing).
pub fn tool_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
