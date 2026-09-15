//! diolingo: download a YouTube video and attach bilingual English + Chinese subtitles.

mod align;
mod burn;
mod captions;
mod ffmpeg;
mod mpv;
mod overlay;
mod play;
mod qwen;
mod subs;
mod ytdlp;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use captions::Cue;
use subs::Order;
use qwen::Qwen;
use ytdlp::{Info, Track, YtDlp, ZhScript};

#[derive(Parser, Debug)]
#[command(name = "diolingo", version, about = "Download a YouTube video and add bilingual EN/ZH subtitles", subcommand_negates_reqs = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// YouTube video or playlist URLs
    #[arg(required = true)]
    urls: Vec<String>,

    /// Base directory: each video's files go to "<OUT>/.diolingo/[<video id>] <title>/" [default: $HOME]
    #[arg(short, long, global = true)]
    out: Option<PathBuf>,

    /// Work directory for downloads and intermediate files [default: "<OUT>/.diolingo/[<video id>] <title>/.work"]
    #[arg(long)]
    work: Option<PathBuf>,

    /// Chinese script to produce
    #[arg(long, value_enum, default_value_t = ZhScript::ZhHant)]
    zh: ZhScript,

    /// Qwen model id or local path for the translation script [default: Qwen/Qwen3-8B]
    #[arg(long)]
    model: Option<String>,

    /// Translation script [default: ~/.diolingo/.scripts/translate_qwen.py, written out by this binary]
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
    #[arg(long, value_enum, default_value_t = Order::EnZh, global = true)]
    order: Order,

    /// Font for the English line in the styled (ASS) subtitles
    #[arg(long, default_value = "Noto Sans", global = true)]
    font_en: String,

    /// Font for the Chinese line in the styled (ASS) subtitles
    #[arg(long, default_value = "Noto Sans CJK TC", global = true)]
    font_zh: String,

    /// Maximum video height to download
    #[arg(long, default_value_t = 1080)]
    max_height: u32,

    /// Only produce subtitle files; skip the video download and mux
    #[arg(long)]
    no_video: bool,

    /// Also run `burn` right away: bilingual .srt/.ass, subtitle tracks in the MKV, hard-subbed copy
    #[arg(long)]
    burn: bool,

    /// Skip the audio-only file (<Title> [id].m4a)
    #[arg(long = "no-audio", action = clap::ArgAction::SetFalse)]
    audio: bool,

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

#[derive(Subcommand, Debug)]
enum Command {
    /// Play a downloaded video's audio with an always-on-top subtitle overlay (needs mpv)
    Play(PlayArgs),
    /// Send an mpv command to the running player, e.g. `ctl sub-seek -1`, `ctl cycle pause`, `ctl add volume 5`
    Ctl(CtlArgs),
    /// List the downloaded videos that `play` can use
    List,
    /// Change the running player's speed: `speed up`, `speed down`, `speed 1.25`, or no argument to print it
    Speed(SpeedArgs),
    /// Rebuild the bilingual .srt/.ass from a folder's .en.srt/.zh.srt (after editing the Chinese)
    Subs(SubsArgs),
    /// `subs`, then mux the subtitle tracks into the MKV and render the hard-subbed copy
    Burn(BurnArgs),
}

#[derive(Args, Debug)]
struct SpeedArgs {
    /// up, down, or a value such as 0.75
    #[arg(allow_hyphen_values = true)]
    value: Option<String>,
}

#[derive(Args, Debug)]
struct SubsArgs {
    /// YouTube video id (the part in [brackets] of the folder name)
    #[arg(allow_hyphen_values = true)]
    target: String,
}

#[derive(Args, Debug)]
struct BurnArgs {
    /// YouTube video id (the part in [brackets] of the folder name)
    #[arg(allow_hyphen_values = true)]
    target: String,

    /// Only the sidecars and the soft subtitle tracks; skip the slow re-encode
    #[arg(long)]
    soft_only: bool,
}

#[derive(Args, Debug)]
struct PlayArgs {
    /// YouTube video id (the part in [brackets] of the folder name)
    #[arg(allow_hyphen_values = true)]
    target: String,

    /// Width of the subtitle bar in pixels (it is centred at the bottom of the screen)
    #[arg(long, default_value_t = 1600)]
    width: i32,

    /// Gap between the bar and the bottom screen edge, in pixels [default: last dragged position, else 40]
    #[arg(long)]
    bottom: Option<i32>,

    /// Distance from the left screen edge, in pixels [default: last dragged position, else centred]
    #[arg(long)]
    left: Option<i32>,

    /// Forget the dragged position and start from the defaults
    #[arg(long)]
    reset_position: bool,

    /// English font size in pixels (Chinese is 90% of it)
    #[arg(long, default_value_t = 40)]
    font_size: i32,

    /// Initial volume in percent (default: mpv's own setting)
    #[arg(long)]
    volume: Option<u32>,

    /// Stop at the end instead of looping the file
    #[arg(long = "no-loop", action = clap::ArgAction::SetFalse)]
    r#loop: bool,

    /// Initial playback speed (practice steps: 0.5 0.75 1 1.25 1.5 1.75 2; pitch is preserved)
    #[arg(long, default_value_t = 1.0)]
    speed: f64,

    /// Shadowing: pause after every sentence long enough to repeat it (click the bar or `ctl cycle pause` to go on early)
    #[arg(long)]
    shadow: bool,

    /// Pause length as a multiple of the sentence's own duration (plus 0.5 s, clamped to 1.5-12 s)
    #[arg(long, default_value_t = 1.0)]
    shadow_ratio: f64,

    /// Longest sentence to build from consecutive cues, in seconds, when there is no punctuation or gap
    #[arg(long, default_value_t = 6.0)]
    shadow_chunk: f64,

    /// Extra argument for mpv, e.g. --mpv-arg=--volume=70 (repeatable)
    #[arg(long = "mpv-arg", allow_hyphen_values = true)]
    mpv_args: Vec<String>,

    /// Print the mpv command instead of running it
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct CtlArgs {
    /// mpv command and its arguments (see mpv's "List of Input Commands")
    #[arg(required = true, allow_hyphen_values = true, trailing_var_arg = true)]
    words: Vec<String>,
}

/// Where a video's files live. All paths are absolute so ffmpeg can run from
/// the work directory without relative outputs ending up in the wrong place.
struct Layout {
    /// Per-video outputs live in `<out_base>/.diolingo/[<id>] <title>/`.
    out_base: PathBuf,
    /// Optional override: work files in `<work_base>/<id>/` instead of `<video dir>/.work/`.
    work_base: Option<PathBuf>,
}

impl Layout {
    /// `<out_base>/.diolingo/[<id>] <sanitized title>/`. An existing directory
    /// whose name starts with `[<id>]` is reused, so a re-run keeps its caches
    /// even if the video was renamed on YouTube.
    fn video_dir(&self, id: &str, title: &str) -> PathBuf {
        let base = self.out_base.join(".diolingo");
        let prefix = format!("[{id}]");
        if let Ok(entries) = fs::read_dir(&base) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir() {
                    return entry.path();
                }
            }
        }
        base.join(format!("{prefix} {}", sanitize(title)))
    }

    fn work_dir(&self, id: &str, title: &str) -> PathBuf {
        match &self.work_base {
            Some(w) => w.join(id),
            None => self.video_dir(id, title).join(".work"),
        }
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[diolingo] {}", msg.as_ref());
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .context("HOME is not set")?;
    let layout = Layout {
        out_base: std::path::absolute(cli.out.clone().unwrap_or_else(|| home.clone())).context("resolving --out")?,
        work_base: cli.work.as_ref().map(std::path::absolute).transpose().context("resolving --work")?,
    };

    match &cli.command {
        Some(Command::Subs(a)) => {
            let dir = play::resolve_dir(&layout.out_base.join(".diolingo"), &a.target)?;
            return burn::write_bilingual(&dir, &burn_opts(&cli, true));
        }
        Some(Command::Burn(b)) => {
            let dir = play::resolve_dir(&layout.out_base.join(".diolingo"), &b.target)?;
            return burn::run(&dir, &burn_opts(&cli, b.soft_only));
        }
        Some(Command::List) => return play::list(&layout.out_base.join(".diolingo")),
        Some(Command::Speed(a)) => return play::speed(a.value.as_deref()),
        Some(Command::Ctl(c)) => return play::ctl(&c.words),
        Some(Command::Play(p)) => {
            return play::run(&play::PlayOpts {
                base: &layout.out_base.join(".diolingo"),
                target: &p.target,
                width: p.width.max(200),
                left: p.left.map(|v| v.max(0)),
                bottom: p.bottom.map(|v| v.max(0)),
                reset_position: p.reset_position,
                font_size: p.font_size.max(8),
                order: cli.order,
                font_en: &cli.font_en,
                font_zh: &cli.font_zh,
                mpv_args: &p.mpv_args,
                volume: p.volume,
                loop_file: p.r#loop,
                speed: p.speed.clamp(0.1, 4.0),
                shadow: p.shadow.then_some(overlay::ShadowOpts {
                    ratio: p.shadow_ratio.max(0.1),
                    chunk_ms: (p.shadow_chunk.max(1.0) * 1000.0) as u64,
                }),
                dry_run: p.dry_run,
            });
        }
        None => {}
    }

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

    let script = match &cli.script {
        Some(s) => std::path::absolute(s).context("resolving --script")?,
        None => qwen::install_script(&home.join(".diolingo").join(".scripts"))?,
    };
    let glossary = qwen::install_glossary(&home.join(".diolingo"), &home.join(".diolingo").join(".scripts"))?;

    let translator = Qwen {
        script,
        glossary: glossary.is_file().then_some(glossary),
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

    fs::create_dir_all(&layout.out_base).with_context(|| format!("creating {}", layout.out_base.display()))?;

    let mut failures = 0usize;
    // Returns true when the video failed; errors are reported, not propagated,
    // so the remaining URLs still get processed.
    let run = |url: &str, info: Info, raw: &str| -> bool {
        let label = format!("{} [{}]", info.title, info.id);
        match process_video(&cli, &layout, &yt, &agent, &translator, url, &info, raw) {
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

#[allow(clippy::too_many_arguments)]
fn process_video(cli: &Cli, layout: &Layout, yt: &YtDlp, agent: &ureq::Agent, translator: &Qwen, url: &str, info: &Info, raw: &str) -> Result<()> {
    let id = info.id.as_str();
    let title = if info.title.is_empty() { id.to_string() } else { info.title.clone() };
    log(format!("== {title} [{id}]"));

    if cli.list_subs {
        eprint!("{}", ytdlp::describe_tracks(info));
        return Ok(());
    }

    let video_dir = layout.video_dir(id, &title);
    let work = layout.work_dir(id, &title);
    if cli.force && work.exists() {
        fs::remove_dir_all(&work).with_context(|| format!("clearing {}", work.display()))?;
    }
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    fs::create_dir_all(&video_dir).with_context(|| format!("creating {}", video_dir.display()))?;
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

    // Sidecars: the English track and the Qwen draft. Everything else
    // (bilingual .srt/.ass, subtitle tracks, hard-sub) is `burn`'s job, so the
    // Chinese can be revised first.
    let stem = format!("{} [{}]", sanitize(&title), id);
    let out = |suffix: &str| video_dir.join(format!("{stem}.{suffix}"));
    subs::write_srt(&out("en.srt"), &en_cues)?;
    subs::write_zh_srt(&out("zh.srt"), &bi)?;
    log(format!("subtitles: {}", out("zh.srt").display()));

    if !cli.no_video {
        let video = yt.download_video(url, &info_path, &work, id, cli.max_height)?;
        let mkv = out("mkv");
        if !mkv.exists() {
            link_or_copy(&video, &mkv)?;
            log(format!("video: {}", mkv.display()));
        }
        if cli.audio {
            let m4a = out("m4a");
            ffmpeg::extract_audio(&video, &m4a)?;
            log(format!("audio: {}", m4a.display()));
        }
        if cli.burn {
            burn::run(&video_dir, &burn_opts(cli, false))?;
        }
    }

    if cli.clean {
        fs::remove_dir_all(&work).with_context(|| format!("removing {}", work.display()))?;
    }
    Ok(())
}

fn burn_opts(cli: &Cli, soft_only: bool) -> burn::BurnOpts<'_> {
    burn::BurnOpts {
        order: cli.order,
        font_en: &cli.font_en,
        font_zh: &cli.font_zh,
        zh_title: match cli.zh {
            ZhScript::ZhHant => "中文（繁體）",
            ZhScript::ZhHans => "中文（简体）",
        },
        soft_only,
    }
}

/// Hard-link `src` as `dst` (free on the same file system), copying if that fails.
fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
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
