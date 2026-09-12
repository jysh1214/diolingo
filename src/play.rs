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
    /// Video id, YouTube URL, part of the title, or a folder path.
    pub target: &'a str,
    pub geometry: (u32, u32),
    pub order: Order,
    pub font_en: &'a str,
    pub font_zh: &'a str,
    pub mpv_args: &'a [String],
    pub dry_run: bool,
}

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
    subs::write_ass(&ass, &cues, opts.order, &AssStyle::player(opts.font_en, opts.font_zh, w, h))?;

    let mut cmd = Command::new("mpv");
    cmd.arg("--title=diolingo")
        .arg("--force-window=immediate")
        .arg(format!("--geometry={w}x{h}"))
        .args(["--ontop", "--border=no", "--keep-open=yes", "--vid=no", "--audio-display=no", "--sub-ass-override=no"])
        .arg(format!("--sub-file={}", ass.display()))
        .args(opts.mpv_args)
        .arg(&audio);

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

/// Find the per-video folder: an existing path, a folder name under `base`, a
/// `[<id>]` prefix (from a bare id or a YouTube URL), or a case-insensitive
/// title substring that matches exactly one folder.
fn resolve_dir(base: &Path, target: &str) -> Result<PathBuf> {
    let as_path = Path::new(target);
    if as_path.is_dir() {
        return std::path::absolute(as_path).context("resolving folder path");
    }
    if base.join(target).is_dir() {
        return Ok(base.join(target));
    }
    let entries: Vec<PathBuf> = fs::read_dir(base)
        .with_context(|| format!("no videos yet: {} does not exist", base.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(id) = youtube_id(target) {
        let prefix = format!("[{id}]");
        if let Some(p) = entries.iter().find(|p| name_of(p).starts_with(&prefix)) {
            return Ok(p.clone());
        }
    }
    let needle = target.to_lowercase();
    let hits: Vec<&PathBuf> = entries.iter().filter(|p| name_of(p).to_lowercase().contains(&needle)).collect();
    match hits.len() {
        0 => bail!("nothing under {} matches {target:?}", base.display()),
        1 => Ok(hits[0].clone()),
        _ => bail!(
            "{target:?} matches several videos, be more specific:\n{}",
            hits.iter().map(|p| format!("  {}", name_of(p))).collect::<Vec<_>>().join("\n")
        ),
    }
}

/// An 11-character YouTube id, taken from a URL (`v=` or `youtu.be/`) or given bare.
fn youtube_id(target: &str) -> Option<String> {
    let is_id = |s: &str| s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let candidate = if let Some(rest) = target.split("v=").nth(1) {
        rest.split(['&', '#']).next().unwrap_or("")
    } else if let Some(rest) = target.split("youtu.be/").nth(1) {
        rest.split(['?', '&', '#']).next().unwrap_or("")
    } else {
        target
    };
    is_id(candidate).then(|| candidate.to_string())
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
    fn extracts_ids() {
        assert_eq!(youtube_id("5C_HPTJg5ek").as_deref(), Some("5C_HPTJg5ek"));
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=5C_HPTJg5ek&t=10").as_deref(), Some("5C_HPTJg5ek"));
        assert_eq!(youtube_id("https://youtu.be/5C_HPTJg5ek?si=x").as_deref(), Some("5C_HPTJg5ek"));
        assert_eq!(youtube_id("rust"), None);
    }

    #[test]
    fn resolves_by_id_then_title() {
        let base = std::env::temp_dir().join(format!("diolingo-play-test-{}", std::process::id()));
        let a = base.join("[5C_HPTJg5ek] Rust in 100 Seconds");
        let b = base.join("[abcdefghijk] Rust and Go");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_eq!(resolve_dir(&base, "5C_HPTJg5ek").unwrap(), a);
        assert_eq!(resolve_dir(&base, "100 seconds").unwrap(), a);
        assert_eq!(resolve_dir(&base, "[abcdefghijk] Rust and Go").unwrap(), b);
        assert!(resolve_dir(&base, "rust").is_err(), "ambiguous");
        assert!(resolve_dir(&base, "nope").is_err());
        fs::remove_dir_all(&base).unwrap();
    }
}
