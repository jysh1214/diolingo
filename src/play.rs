//! `diolingo play`: background audio with a floating bilingual-subtitle window (mpv).

use crate::align::BiCue;
use crate::captions;
use crate::subs::{self, AssStyle, Order};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct PlayOpts<'a> {
    /// `<out>/.diolingo`, where the per-video folders live.
    pub base: &'a Path,
    /// YouTube video id.
    pub target: &'a str,
    pub geometry: (u32, u32),
    /// English font size in pixels.
    pub font_size: u32,
    pub order: Order,
    pub font_en: &'a str,
    pub font_zh: &'a str,
    pub mpv_args: &'a [String],
    /// Initial volume in percent; `None` leaves mpv's own default.
    pub volume: Option<u32>,
    pub dry_run: bool,
}

/// Bindings added on top of mpv's defaults: the mouse wheel over the bar
/// changes the volume instead of seeking (keys 9 / 0 and m still work).
const INPUT_CONF: &str = "\
# written by diolingo play; mpv's built-in bindings stay active
WHEEL_UP    add volume 5
WHEEL_DOWN  add volume -5
";

pub fn run(opts: &PlayOpts) -> Result<()> {
    let dir = resolve_dir(opts.base, opts.target)?;
    let audio = find_file(&dir, ".m4a")
        .or_else(|| find_file(&dir, ".mkv").filter(|p| !p.to_string_lossy().ends_with(".hardsub.mkv")))
        .with_context(|| format!("no .m4a or .mkv in {}", dir.display()))?;
    let en_srt = find_file(&dir, ".en.srt").with_context(|| format!("no .en.srt in {}", dir.display()))?;
    let zh_srt = find_file(&dir, ".zh.srt").with_context(|| format!("no .zh.srt in {}", dir.display()))?;

    let cues = load_bilingual(&en_srt, &zh_srt)?;
    let (w, h) = opts.geometry;
    let ass = dir.join(".player.ass");
    subs::write_ass(&ass, &cues, opts.order, &AssStyle::player(opts.font_en, opts.font_zh, w, h, opts.font_size))?;
    let input_conf = write_input_conf(opts.base)?;

    let mut cmd = Command::new("mpv");
    cmd.arg("--title=diolingo")
        .arg("--force-window=immediate")
        .arg(format!("--geometry={w}x{h}"))
        // With no video track mpv places a 16:9 "video" area inside the window and
        // renders subtitles into it; force-margins makes libass use the whole window.
        .args(["--ontop", "--border=no", "--keep-open=yes", "--vid=no", "--audio-display=no"])
        .args(["--sub-ass-override=no", "--sub-ass-force-margins=yes", "--sub-use-margins=yes"])
        .arg(format!("--sub-file={}", ass.display()))
        .arg(format!("--input-conf={}", input_conf.display()));
    if let Some(v) = opts.volume {
        cmd.arg(format!("--volume={v}"));
    }
    cmd.args(opts.mpv_args).arg(&audio);

    if opts.dry_run {
        println!("{}", shell_words(&cmd));
        return Ok(());
    }
    eprintln!("[diolingo] playing {}", audio.display());
    let status = cmd.status().context("running mpv (install it with: sudo pacman -S mpv)")?;
    if !status.success() {
        bail!("mpv exited with {status}");
    }
    Ok(())
}

/// Find the per-video folder `[<id>] <title>` under `base`.
fn resolve_dir(base: &Path, id: &str) -> Result<PathBuf> {
    if !is_video_id(id) {
        bail!("{id:?} is not a YouTube video id (11 characters, e.g. 5C_HPTJg5ek)");
    }
    let prefix = format!("[{id}]");
    fs::read_dir(base)
        .with_context(|| format!("no videos yet: {} does not exist", base.display()))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
        .with_context(|| format!("no folder for video {id} under {}", base.display()))
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `<base>/.mpv-input.conf`: the user's own `~/.config/mpv/input.conf` (if
/// any, so `--input-conf` does not hide it) followed by the diolingo bindings.
fn write_input_conf(base: &Path) -> Result<PathBuf> {
    let user_conf = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("mpv").join("input.conf"));
    let mut content = String::new();
    if let Some(user) = user_conf.filter(|p| p.is_file()) {
        content.push_str(&fs::read_to_string(&user).with_context(|| format!("reading {}", user.display()))?);
        content.push('\n');
    }
    content.push_str(INPUT_CONF);
    fs::create_dir_all(base)?;
    let path = base.join(".mpv-input.conf");
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn find_file(dir: &Path, suffix: &str) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(suffix)))
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Rebuild bilingual cues from the two single-language sidecars, which share
/// their timings; a cue with no Chinese line is kept with an empty `zh`.
fn load_bilingual(en_srt: &Path, zh_srt: &Path) -> Result<Vec<BiCue>> {
    let en = captions::parse_srt(&fs::read_to_string(en_srt)?).with_context(|| format!("parsing {}", en_srt.display()))?;
    let zh = captions::parse_srt(&fs::read_to_string(zh_srt)?).with_context(|| format!("parsing {}", zh_srt.display()))?;
    let zh_by_start: HashMap<u64, String> = zh.into_iter().map(|c| (c.start_ms, c.text)).collect();
    Ok(en
        .into_iter()
        .map(|c| {
            let zh = zh_by_start.get(&c.start_ms).cloned().unwrap_or_default();
            BiCue { start_ms: c.start_ms, end_ms: c.end_ms, en: c.text, zh }
        })
        .collect())
}

fn shell_words(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| {
            let s = a.to_string_lossy();
            if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:,".contains(c)) { s.into_owned() } else { format!("'{}'", s.replace('\'', "'\\''")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_by_id_only() {
        let base = std::env::temp_dir().join(format!("diolingo-play-test-{}", std::process::id()));
        let a = base.join("[5C_HPTJg5ek] Rust in 100 Seconds");
        fs::create_dir_all(&a).unwrap();
        assert_eq!(resolve_dir(&base, "5C_HPTJg5ek").unwrap(), a);
        assert!(resolve_dir(&base, "abcdefghijk").is_err(), "unknown id");
        assert!(resolve_dir(&base, "100 seconds").is_err(), "titles are not accepted");
        assert!(resolve_dir(&base, "https://youtu.be/5C_HPTJg5ek").is_err(), "URLs are not accepted");
        fs::remove_dir_all(&base).unwrap();
    }
}
