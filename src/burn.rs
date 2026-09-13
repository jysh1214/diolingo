//! `diolingo burn`: from a folder's `.en.srt` and `.zh.srt`, build the bilingual
//! `.srt` and `.ass`, mux the subtitle tracks into the MKV and render the
//! hard-subbed copy. Purely local (ffmpeg); no download, no translation.

use crate::ffmpeg;
use crate::play;
use crate::subs::{self, AssStyle, Order};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub struct BurnOpts<'a> {
    pub order: Order,
    pub font_en: &'a str,
    pub font_zh: &'a str,
    /// Track title for the Chinese subtitle stream.
    pub zh_title: &'a str,
    /// Only the sidecars and the soft subtitle tracks; skip the re-encode.
    pub soft_only: bool,
}

pub fn run(dir: &Path, opts: &BurnOpts) -> Result<()> {
    let en_srt = play::find_file(dir, ".en.srt").with_context(|| format!("no .en.srt in {}", dir.display()))?;
    let zh_srt = play::find_file(dir, ".zh.srt").with_context(|| format!("no .zh.srt in {}", dir.display()))?;
    let stem = en_srt
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".en.srt"))
        .context("odd .en.srt file name")?
        .to_string();
    let out = |suffix: &str| dir.join(format!("{stem}.{suffix}"));

    let bi = play::load_bilingual(&en_srt, &zh_srt)?;
    let with_zh = bi.iter().filter(|c| !c.zh.is_empty()).count();
    eprintln!("[diolingo] bilingual: {} cues, {with_zh} with Chinese", bi.len());
    let (bi_srt, bi_ass) = (out("srt"), out("ass"));
    subs::write_bilingual_srt(&bi_srt, &bi, opts.order)?;
    subs::write_ass(&bi_ass, &bi, opts.order, &AssStyle::video(opts.font_en, opts.font_zh))?;
    eprintln!("[diolingo] subtitles: {}", bi_srt.display());

    // Source video: the untouched download in .work/, else the folder's own MKV
    // (its old subtitle tracks are dropped because only video/audio are mapped).
    let mkv = out("mkv");
    let work_video = play::folder_id(dir).map(|id| dir.join(".work").join(format!("{id}.mkv"))).filter(|p| p.is_file());
    let source = work_video
        .or_else(|| mkv.is_file().then(|| mkv.clone()))
        .with_context(|| format!("no video in {} (run diolingo on the URL first)", dir.display()))?;

    let tracks = [
        ffmpeg::SubTrack { path: &bi_ass, lang: "mul", title: "English + 中文 (styled)", default: true },
        ffmpeg::SubTrack { path: &bi_srt, lang: "mul", title: "English + 中文", default: false },
        ffmpeg::SubTrack { path: &en_srt, lang: "eng", title: "English", default: false },
        ffmpeg::SubTrack { path: &zh_srt, lang: "chi", title: opts.zh_title, default: false },
    ];
    let tmp = out("mux.tmp.mkv");
    eprintln!("[diolingo] muxing subtitle tracks into MKV");
    ffmpeg::mux(&source, &tracks, &tmp)?;
    fs::rename(&tmp, &mkv).with_context(|| format!("replacing {}", mkv.display()))?;
    eprintln!("[diolingo] video: {}", mkv.display());

    if !opts.soft_only {
        let hard = out("hardsub.mkv");
        // ffmpeg's filter argument cannot take the bracketed folder/file names
        // unescaped, so the ASS is burned from a plainly named copy in `dir`.
        let burn_ass = dir.join(".burn.ass");
        fs::copy(&bi_ass, &burn_ass)?;
        eprintln!("[diolingo] burning styled subtitles into the picture (libx264 re-encode)");
        let result = ffmpeg::burn(dir, &source, ".burn.ass", &hard);
        let _ = fs::remove_file(&burn_ass);
        result?;
        eprintln!("[diolingo] hard-subbed video: {}", hard.display());
    }
    Ok(())
}
