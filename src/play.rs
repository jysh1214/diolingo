//! `diolingo play`: headless mpv for the audio plus a layer-shell subtitle
//! overlay; `diolingo ctl`: send a command to that player.

use crate::align::BiCue;
use crate::captions;
use crate::mpv;
use crate::overlay::{self, OverlayOpts, Position};
use crate::subs::Order;
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub struct PlayOpts<'a> {
    /// `<out>/.diolingo`, where the per-video folders live.
    pub base: &'a Path,
    /// YouTube video id.
    pub target: &'a str,
    pub width: i32,
    /// Explicit `--left` / `--bottom`; otherwise the saved position, then the default.
    pub left: Option<i32>,
    pub bottom: Option<i32>,
    pub reset_position: bool,
    pub font_size: i32,
    pub order: Order,
    pub font_en: &'a str,
    pub font_zh: &'a str,
    pub mpv_args: &'a [String],
    /// Initial volume in percent; `None` leaves mpv's own default.
    pub volume: Option<u32>,
    /// Restart the file when it ends.
    pub loop_file: bool,
    pub dry_run: bool,
}

pub fn run(opts: &PlayOpts) -> Result<()> {
    let dir = resolve_dir(opts.base, opts.target)?;
    let audio = find_file(&dir, ".m4a")
        .or_else(|| find_file(&dir, ".mkv").filter(|p| !p.to_string_lossy().ends_with(".hardsub.mkv")))
        .with_context(|| format!("no .m4a or .mkv in {}", dir.display()))?;
    let en_srt = find_file(&dir, ".en.srt").with_context(|| format!("no .en.srt in {}", dir.display()))?;
    let zh_srt = find_file(&dir, ".zh.srt").with_context(|| format!("no .zh.srt in {}", dir.display()))?;
    let bi_srt = find_bilingual_srt(&dir).with_context(|| format!("no bilingual .srt in {}", dir.display()))?;
    let cues = load_bilingual(&en_srt, &zh_srt)?;

    let socket = mpv::socket_path();
    let mut cmd = Command::new("mpv");
    cmd.args(["--no-video", "--force-window=no", "--keep-open=no", "--really-quiet", "--no-terminal"])
        .arg(format!("--input-ipc-server={}", socket.display()))
        // Loaded so `sub-seek` (previous/next line) works over `diolingo ctl`.
        .arg(format!("--sub-file={}", bi_srt.display()));
    if let Some(v) = opts.volume {
        cmd.arg(format!("--volume={v}"));
    }
    if opts.loop_file {
        cmd.arg("--loop-file=inf");
    }
    cmd.args(opts.mpv_args).arg(&audio);
    if opts.dry_run {
        println!("{}", shell_words(&cmd));
        return Ok(());
    }

    stop_previous(&socket);
    let mut child = cmd.stdin(Stdio::null()).spawn().context("starting mpv (install it with: sudo pacman -S mpv)")?;
    if let Err(e) = wait_for_socket(&socket, &mut child) {
        let _ = child.kill();
        return Err(e);
    }
    eprintln!("[diolingo] playing {}", audio.display());

    let position_file = opts.base.join(".overlay-position");
    if opts.reset_position {
        let _ = fs::remove_file(&position_file);
    }
    let mut position = Position::load(&position_file).unwrap_or(Position::DEFAULT);
    if opts.left.is_some() {
        position.left = opts.left;
    }
    if let Some(b) = opts.bottom {
        position.bottom = b;
    }
    let result = overlay::run(
        cues,
        OverlayOpts {
            width: opts.width,
            font_size: opts.font_size,
            position,
            position_file,
            font_en: opts.font_en.to_string(),
            font_zh: opts.font_zh.to_string(),
            order: opts.order,
        },
        &socket,
    );

    // The overlay returned (mpv finished, or we were interrupted): stop mpv.
    if child.try_wait()?.is_none() {
        if let Ok(mut c) = mpv::Client::connect(&socket) {
            let _ = c.command(vec![json!("quit")]);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            sleep(Duration::from_millis(50));
        }
        if child.try_wait()?.is_none() {
            let _ = child.kill();
        }
    }
    let _ = child.wait();
    let _ = fs::remove_file(&socket);
    result
}

/// `diolingo list`: every `[<id>] <title>` folder under `base`, newest first,
/// with the audio length and a note when something needed by `play` is missing.
pub fn list(base: &Path) -> Result<()> {
    let mut rows: Vec<(std::time::SystemTime, String, String)> = Vec::new();
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => {
            println!("no videos yet ({} does not exist)", base.display());
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((id, title)) = parse_folder_name(&name) else { continue };
        if !path.is_dir() {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let has_audio = find_file(&path, ".m4a").is_some() || find_file(&path, ".mkv").is_some();
        let en = find_file(&path, ".en.srt");
        let has_zh = find_file(&path, ".zh.srt").is_some();
        let length = en
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| captions::parse_srt(&t).ok())
            .and_then(|cues| cues.last().map(|c| c.end_ms))
            .map(format_length)
            .unwrap_or_else(|| "--:--".to_string());
        let note = match (has_audio, en.is_some() && has_zh) {
            (true, true) => String::new(),
            (false, true) => "  (no audio: run diolingo on it again)".to_string(),
            (true, false) => "  (no subtitles)".to_string(),
            (false, false) => "  (incomplete)".to_string(),
        };
        rows.push((modified, id.to_string(), format!("{length:>6}  {title}{note}")));
    }
    if rows.is_empty() {
        println!("no videos yet under {}", base.display());
        return Ok(());
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (_, id, rest) in rows {
        println!("{id}  {rest}");
    }
    Ok(())
}

/// Split `[<id>] <title>` into its parts.
fn parse_folder_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix('[')?;
    let (id, title) = rest.split_once(']')?;
    is_video_id(id).then(|| (id, title.trim()))
}

fn format_length(ms: u64) -> String {
    let secs = ms.div_ceil(1000);
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// `diolingo ctl <mpv command...>`: forward one command to the running player.
pub fn ctl(words: &[String]) -> Result<()> {
    let mut client = mpv::Client::connect(&mpv::socket_path())?;
    let reply = client.command(mpv::parse_ctl_args(words))?;
    if !reply.is_null() {
        println!("{reply}");
    }
    Ok(())
}

fn stop_previous(socket: &Path) {
    if let Ok(mut c) = mpv::Client::connect(socket) {
        eprintln!("[diolingo] stopping the previous player");
        let _ = c.command(vec![json!("quit")]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while socket.exists() && Instant::now() < deadline {
            sleep(Duration::from_millis(50));
        }
    }
    let _ = fs::remove_file(socket);
}

fn wait_for_socket(socket: &Path, child: &mut std::process::Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("mpv exited before opening its IPC socket ({status})");
        }
        if Instant::now() > deadline {
            bail!("mpv did not open {} within 10s", socket.display());
        }
        sleep(Duration::from_millis(100));
    }
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

/// `<Title> [id].srt`, i.e. the `.srt` that is neither `.en.srt` nor `.zh.srt`.
fn find_bilingual_srt(dir: &Path) -> Option<PathBuf> {
    let mut hits: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            p.is_file() && n.ends_with(".srt") && !n.ends_with(".en.srt") && !n.ends_with(".zh.srt")
        })
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
    fn folder_names_and_lengths() {
        assert_eq!(parse_folder_name("[5C_HPTJg5ek] Rust in 100 Seconds"), Some(("5C_HPTJg5ek", "Rust in 100 Seconds")));
        assert_eq!(parse_folder_name("[5C_HPTJg5ek]"), Some(("5C_HPTJg5ek", "")));
        assert_eq!(parse_folder_name("notes"), None);
        assert_eq!(parse_folder_name("[short] x"), None);
        assert_eq!(format_length(148_631), "02:29");
        assert_eq!(format_length(3_723_000), "1:02:03");
    }

    #[test]
    fn resolves_by_id_only() {
        let base = std::env::temp_dir().join(format!("diolingo-play-test-{}", std::process::id()));
        let a = base.join("[5C_HPTJg5ek] Rust in 100 Seconds");
        fs::create_dir_all(&a).unwrap();
        assert_eq!(resolve_dir(&base, "5C_HPTJg5ek").unwrap(), a);
        assert!(resolve_dir(&base, "abcdefghijk").is_err(), "unknown id");
        assert!(resolve_dir(&base, "100 seconds").is_err(), "titles are not accepted");
        fs::remove_dir_all(&base).unwrap();
    }
}
