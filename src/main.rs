//! diolingo: download a YouTube video and attach bilingual English + Chinese subtitles.

mod align;
mod captions;
mod ffmpeg;
mod qwen;
mod subs;
mod ytdlp;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use captions::Cue;
use subs::{AssStyle, Order};
use qwen::Qwen;
use ytdlp::{Info, Track, YtDlp, ZhScript};

#[derive(Parser, Debug)]
#[command(name = "diolingo", version, about = "Download a YouTube video and add bilingual EN/ZH subtitles")]
struct Cli {
    /// YouTube video or playlist URLs
    #[arg(required = true)]
    urls: Vec<String>,

    /// Output directory for the final .mkv and subtitle sidecars
    #[arg(short, long, default_value = ".")]
    out: PathBuf,

    /// Work directory for downloads and intermediate files [default: <out>/.diolingo]
    #[arg(long)]
    work: Option<PathBuf>,

    /// Chinese script to produce
    #[arg(long, value_enum, default_value_t = ZhScript::ZhHant)]
    zh: ZhScript,

    /// Qwen model id or local path for the translation script [default: Qwen/Qwen3-8B]
    #[arg(long)]
    model: Option<String>,

    /// Translation script [default: bundled scripts/translate_qwen.py]
    #[arg(long)]
    script: Option<PathBuf>,

    /// Python interpreter for the translation script (default: `uv run --script`, which installs torch/transformers on first use)
    #[arg(long)]
    python: Option<PathBuf>,

    /// Extra argument for the translation script, e.g. --qwen-arg=--quant --qwen-arg=4bit (repeatable)
    #[arg(long = "qwen-arg", allow_hyphen_values = true)]
    qwen_args: Vec<String>,

    /// Subtitle lines per translation request (small keeps line numbering aligned)
    #[arg(long, default_value_t = 16)]
    batch: usize,

    /// Force a specific YouTube English track key (e.g. en, en-orig)
    #[arg(long)]
    en_lang: Option<String>,

    /// Line order inside each bilingual cue
    #[arg(long, value_enum, default_value_t = Order::EnZh)]
    order: Order,

    /// Font for the English line in the styled (ASS) subtitles
    #[arg(long, default_value = "Noto Sans")]
    font_en: String,

    /// Font for the Chinese line in the styled (ASS) subtitles
    #[arg(long, default_value = "Noto Sans CJK TC")]
    font_zh: String,

    /// Maximum video height to download
    #[arg(long, default_value_t = 1080)]
    max_height: u32,

    /// Only produce subtitle files; skip the video download and mux
    #[arg(long)]
    no_video: bool,

    /// Skip the hard-subbed copy (<Title> [id].hardsub.mkv, a libx264 re-encode with the styled subtitles burned in)
    #[arg(long = "no-burn", action = clap::ArgAction::SetFalse)]
    burn: bool,

    /// Print the available caption tracks and exit
    #[arg(long)]
    list_subs: bool,

    /// Let yt-dlp read cookies from a browser (e.g. firefox, chromium)
    #[arg(long)]
    cookies_from_browser: Option<String>,

    /// Netscape cookies file for yt-dlp
    #[arg(long)]
    cookies: Option<PathBuf>,

    /// Extra argument passed verbatim to yt-dlp, e.g. --yt-dlp-arg=--limit-rate --yt-dlp-arg=2M (repeatable)
    #[arg(long = "yt-dlp-arg", allow_hyphen_values = true)]
    ytdlp_args: Vec<String>,

    /// Ignore cached captions and downloads in the work directory
    #[arg(long)]
    force: bool,

    /// Delete the per-video work directory after a successful run
    #[arg(long)]
    clean: bool,
}

impl Cli {
    fn work_dir(&self) -> PathBuf {
        self.work.clone().unwrap_or_else(|| self.out.join(".diolingo"))
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[diolingo] {}", msg.as_ref());
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut common_args = Vec::new();
    if let Some(b) = &cli.cookies_from_browser {
        common_args.extend(["--cookies-from-browser".to_string(), b.clone()]);
    }
    if let Some(c) = &cli.cookies {
        common_args.extend(["--cookies".to_string(), c.to_string_lossy().into_owned()]);
    }
    common_args.extend(cli.ytdlp_args.iter().cloned());
    let yt = YtDlp { bin: "yt-dlp".into(), common_args };
    let version = yt.check()?;
    if !cli.no_video && !cli.list_subs {
        ffmpeg::check()?;
    }
    log(format!("yt-dlp {version}"));

    let translator = Qwen {
        script: cli.script.clone().unwrap_or_else(|| PathBuf::from(qwen::DEFAULT_SCRIPT)),
        python: cli.python.clone(),
        model: cli.model.clone(),
        batch_lines: cli.batch,
        extra_args: cli.qwen_args.clone(),
    };
    if !cli.list_subs {
        translator.check()?;
    }
    log(format!("translator: {}", translator.describe()));

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into();

    fs::create_dir_all(&cli.out).with_context(|| format!("creating {}", cli.out.display()))?;

    let mut failures = 0usize;
    // Returns true when the video failed; errors are reported, not propagated,
    // so the remaining URLs still get processed.
    let run = |url: &str, info: Info, raw: &str| -> bool {
        let label = format!("{} [{}]", info.title, info.id);
        match process_video(&cli, &yt, &agent, &translator, url, &info, raw) {
            Ok(()) => false,
            Err(e) => {
                eprintln!("[diolingo] ERROR {label}: {e:#}");
                true
            }
        }
    };

    for url in &cli.urls {
        let (info, raw) = yt.probe(url)?;
        if info.is_playlist() {
            let entries = info.entries.unwrap_or_default();
            log(format!("playlist \"{}\": {} videos", info.title, entries.len()));
            for entry in entries {
                let vurl = entry.video_url();
                match yt.probe(&vurl) {
                    Ok((vinfo, vraw)) => failures += usize::from(run(&vurl, vinfo, &vraw)),
                    Err(e) => {
                        failures += 1;
                        eprintln!("[diolingo] ERROR {vurl}: {e:#}");
                    }
                }
            }
        } else {
            failures += usize::from(run(url, info, &raw));
        }
    }
    if failures > 0 {
        bail!("{failures} video(s) failed");
    }
    Ok(())
}

fn process_video(cli: &Cli, yt: &YtDlp, agent: &ureq::Agent, translator: &Qwen, url: &str, info: &Info, raw: &str) -> Result<()> {
    let id = info.id.as_str();
    let title = if info.title.is_empty() { id.to_string() } else { info.title.clone() };
    log(format!("== {title} [{id}]"));

    if cli.list_subs {
        eprint!("{}", ytdlp::describe_tracks(info));
        return Ok(());
    }

    let work = cli.work_dir().join(id);
    if cli.force && work.exists() {
        fs::remove_dir_all(&work).with_context(|| format!("clearing {}", work.display()))?;
    }
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let info_path = work.join(format!("{id}.info.json"));
    fs::write(&info_path, raw)?;

    // English track (the timeline).
    let en_track = ytdlp::pick_english(info, cli.en_lang.as_deref())?
        .ok_or_else(|| anyhow!("no English subtitles or auto captions (try --list-subs / --en-lang)"))?;
    log(format!("english: {}", en_track.describe()));
    let en_cues = load_track(agent, &en_track, &work, id)?;
    if en_cues.is_empty() {
        bail!("English track {} has no cues", en_track.key);
    }
    log(format!("english: {} cues", en_cues.len()));

    // Chinese: translate the English cues 1:1 with the local Qwen model.
    let texts: Vec<String> = en_cues.iter().map(|c| c.text.clone()).collect();
    log(format!("translating {} lines to {} with local Qwen", texts.len(), cli.zh.tag()));
    let zh = translator.translate(&work, id, &texts, &title, cli.zh.llm_name())?;
    let bi = align::zip(&en_cues, &zh);
    let with_zh = bi.iter().filter(|c| !c.zh.is_empty()).count();
    log(format!("bilingual: {} cues, {with_zh} with Chinese", bi.len()));

    // Subtitle files (work dir, id-based names) then copies with the title.
    let w = |suffix: &str| work.join(format!("{id}.{suffix}"));
    let (bi_srt, bi_ass, en_srt, zh_srt) = (w("bi.srt"), w("bi.ass"), w("en.srt"), w(&format!("{}.srt", cli.zh.tag())));
    subs::write_bilingual_srt(&bi_srt, &bi, cli.order)?;
    subs::write_ass(&bi_ass, &bi, cli.order, &AssStyle { font_en: &cli.font_en, font_zh: &cli.font_zh })?;
    subs::write_srt(&en_srt, &en_cues)?;
    subs::write_zh_srt(&zh_srt, &bi)?;

    let stem = format!("{} [{}]", sanitize(&title), id);
    let out = |suffix: &str| cli.out.join(format!("{stem}.{suffix}"));
    fs::copy(&bi_srt, out("srt"))?;
    fs::copy(&bi_ass, out("ass"))?;
    fs::copy(&en_srt, out("en.srt"))?;
    fs::copy(&zh_srt, out("zh.srt"))?;
    log(format!("subtitles: {}", out("srt").display()));

    if !cli.no_video {
        let video = yt.download_video(url, &info_path, &work, id, cli.max_height)?;
        let zh_title = match cli.zh {
            ZhScript::ZhHant => "中文（繁體）",
            ZhScript::ZhHans => "中文（简体）",
        };
        let tracks = [
            ffmpeg::SubTrack { path: &bi_ass, lang: "mul", title: "English + 中文 (styled)", default: true },
            ffmpeg::SubTrack { path: &bi_srt, lang: "mul", title: "English + 中文", default: false },
            ffmpeg::SubTrack { path: &en_srt, lang: "eng", title: "English", default: false },
            ffmpeg::SubTrack { path: &zh_srt, lang: "chi", title: zh_title, default: false },
        ];
        let mkv = out("mkv");
        log("muxing subtitle tracks into MKV");
        ffmpeg::mux(&video, &tracks, &mkv)?;
        log(format!("video: {}", mkv.display()));
        if cli.burn {
            let hard = out("hardsub.mkv");
            log("burning styled subtitles into the picture (libx264 re-encode)");
            let ass_name = bi_ass.file_name().and_then(|n| n.to_str()).context("ass file name")?;
            ffmpeg::burn(&work, &video.canonicalize()?, ass_name, &hard.canonicalize().unwrap_or(hard.clone()))?;
            log(format!("hard-subbed video: {}", hard.display()));
        }
    }

    if cli.clean {
        fs::remove_dir_all(&work).with_context(|| format!("removing {}", work.display()))?;
    }
    Ok(())
}

/// Fetch (or reuse) a caption document and parse it into cues.
fn load_track(agent: &ureq::Agent, track: &Track, work: &Path, id: &str) -> Result<Vec<Cue>> {
    let kind = if track.auto { "auto" } else { "manual" };
    let cache = work.join(format!("{id}.{}.{kind}.{}", track.key, track.ext));
    let text = if cache.is_file() {
        fs::read_to_string(&cache)?
    } else {
        let text = ytdlp::fetch_caption(agent, &track.url).with_context(|| format!("fetching {} captions", track.key))?;
        fs::write(&cache, &text)?;
        text
    };
    captions::parse(&track.ext, &text).with_context(|| format!("parsing {}", cache.display()))
}

/// Make a title safe as a file name; keeps Unicode, drops path/reserved chars.
fn sanitize(title: &str) -> String {
    let mapped: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let s: String = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c| c == '.' || c == ' ');
    let s: String = s.chars().take(100).collect();
    if s.is_empty() { "video".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_titles() {
        assert_eq!(sanitize("Rust: in 100 Seconds?"), "Rust_ in 100 Seconds_");
        assert_eq!(sanitize("  中文  標題 / test  "), "中文 標題 _ test");
        assert_eq!(sanitize("..."), "video");
    }
}
