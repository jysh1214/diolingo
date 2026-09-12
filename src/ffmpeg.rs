//! ffmpeg wrappers: soft-mux subtitle tracks into MKV, or burn the ASS in.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

pub struct SubTrack<'a> {
    pub path: &'a Path,
    pub lang: &'a str,
    pub title: &'a str,
    pub default: bool,
}

pub fn check() -> Result<()> {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .context("ffmpeg not found on PATH")?;
    Ok(())
}

/// Copy video/audio streams and attach every subtitle file as its own track.
pub fn mux(video: &Path, subs: &[SubTrack], out: &Path) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"]).arg(video);
    for s in subs {
        cmd.arg("-i").arg(s.path);
    }
    cmd.args(["-map", "0:v:0", "-map", "0:a?"]);
    for i in 0..subs.len() {
        cmd.arg("-map").arg((i + 1).to_string());
    }
    cmd.args(["-c", "copy"]);
    for (i, s) in subs.iter().enumerate() {
        cmd.arg(format!("-metadata:s:s:{i}")).arg(format!("language={}", s.lang));
        cmd.arg(format!("-metadata:s:s:{i}")).arg(format!("title={}", s.title));
        cmd.arg(format!("-disposition:s:{i}")).arg(if s.default { "default" } else { "0" });
    }
    cmd.arg(out);
    let status = cmd.status().context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg mux failed ({status})");
    }
    Ok(())
}

/// Write the first audio stream to an `.m4a`: stream-copied when it is already
/// AAC (YouTube's usual m4a track), otherwise transcoded to AAC.
pub fn extract_audio(video: &Path, out: &Path) -> Result<()> {
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(video)
        .output()
        .context("running ffprobe")?;
    let codec = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    if codec.is_empty() {
        bail!("no audio stream in {}", video.display());
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"]).arg(video).args(["-map", "0:a:0", "-vn"]);
    if codec == "aac" {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(out);
    let status = cmd.status().context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg audio extraction failed ({status})");
    }
    Ok(())
}

/// Re-encode the video with the ASS file rendered into the picture.
/// `ass_name` must be a plain file name inside `work_dir` (no path escaping needed).
pub fn burn(work_dir: &Path, video: &Path, ass_name: &str, out: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .current_dir(work_dir)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-stats", "-i"])
        .arg(video)
        .args(["-map", "0:v:0", "-map", "0:a?", "-vf"])
        .arg(format!("ass={ass_name}"))
        .args(["-c:v", "libx264", "-preset", "medium", "-crf", "20", "-pix_fmt", "yuv420p", "-c:a", "copy"])
        .arg(out)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!("ffmpeg burn-in failed ({status})");
    }
    Ok(())
}
