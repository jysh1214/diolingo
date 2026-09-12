//! yt-dlp wrapper: probe metadata, pick caption tracks, fetch captions, download video.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct SubFormat {
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

pub type SubMap = BTreeMap<String, Vec<SubFormat>>;

/// The subset of `yt-dlp -J` output we use. A playlist probe has `_type ==
/// "playlist"` and `entries`; a video probe has caption maps.
#[derive(Debug, Deserialize)]
pub struct Info {
    #[serde(rename = "_type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub subtitles: Option<SubMap>,
    #[serde(default)]
    pub automatic_captions: Option<SubMap>,
    #[serde(default)]
    pub entries: Option<Vec<Info>>,
}

impl Info {
    pub fn is_playlist(&self) -> bool {
        self.kind.as_deref() == Some("playlist")
    }

    pub fn video_url(&self) -> String {
        self.url.clone().unwrap_or_else(|| format!("https://www.youtube.com/watch?v={}", self.id))
    }
}

/// Chinese script to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ZhScript {
    /// Traditional Chinese
    ZhHant,
    /// Simplified Chinese
    ZhHans,
}

impl ZhScript {
    pub fn tag(self) -> &'static str {
        match self {
            ZhScript::ZhHant => "zh-Hant",
            ZhScript::ZhHans => "zh-Hans",
        }
    }

    /// How the target language is described to an LLM.
    pub fn llm_name(self) -> &'static str {
        match self {
            ZhScript::ZhHant => "Traditional Chinese as used in Taiwan (台灣繁體中文，使用台灣慣用語)",
            ZhScript::ZhHans => "Simplified Chinese (简体中文)",
        }
    }
}

/// One selected caption track.
#[derive(Debug, Clone)]
pub struct Track {
    pub key: String,
    pub auto: bool,
    pub name: String,
    pub ext: String,
    pub url: String,
}

impl Track {
    pub fn describe(&self) -> String {
        let kind = if self.auto { "auto" } else { "manual" };
        if self.name.is_empty() { format!("{} [{kind}]", self.key) } else { format!("{} [{kind}, {}]", self.key, self.name) }
    }
}

pub fn find_track(info: &Info, key: &str, auto: bool) -> Option<Track> {
    let map = if auto { info.automatic_captions.as_ref() } else { info.subtitles.as_ref() }?;
    let fmts = map.get(key)?;
    let f = ["json3", "vtt", "srt"].iter().find_map(|ext| fmts.iter().find(|f| f.ext == *ext))?;
    let url = f.url.clone()?;
    Some(Track { key: key.to_string(), auto, name: f.name.clone().unwrap_or_default(), ext: f.ext.clone(), url })
}

fn pick_first(info: &Info, candidates: &[(&str, bool)]) -> Option<Track> {
    candidates.iter().find_map(|(k, a)| find_track(info, k, *a))
}

fn manual_matching(info: &Info, pred: impl Fn(&str) -> bool) -> Option<Track> {
    info.subtitles.as_ref()?.keys().find(|k| pred(k)).and_then(|k| find_track(info, k, false))
}

fn forced(info: &Info, key: &str) -> Result<Track> {
    find_track(info, key, false)
        .or_else(|| find_track(info, key, true))
        .with_context(|| format!("no subtitle track {key:?}; run with --list-subs to see what exists"))
}

/// Manual English first, then the original auto captions, then auto-translated English.
pub fn pick_english(info: &Info, force: Option<&str>) -> Result<Option<Track>> {
    if let Some(k) = force {
        return forced(info, k).map(Some);
    }
    Ok(pick_first(info, &[("en", false), ("en-US", false), ("en-GB", false)])
        .or_else(|| manual_matching(info, |k| k == "en" || k.starts_with("en-")))
        .or_else(|| pick_first(info, &[("en-orig", true), ("en", true), ("en-US", true)])))
}

/// Human-readable listing of every caption track yt-dlp reported.
pub fn describe_tracks(info: &Info) -> String {
    let mut s = String::new();
    let manual: Vec<String> = info
        .subtitles
        .iter()
        .flatten()
        .map(|(k, v)| match v.first().and_then(|f| f.name.clone()) {
            Some(n) => format!("{k} ({n})"),
            None => k.clone(),
        })
        .collect();
    s.push_str(&format!("  manual subtitles ({}): {}\n", manual.len(), if manual.is_empty() { "none".to_string() } else { manual.join(", ") }));
    let auto: Vec<&String> = info.automatic_captions.iter().flatten().map(|(k, _)| k).collect();
    let shown: Vec<&str> = auto.iter().filter(|k| k.starts_with("en")).map(|k| k.as_str()).collect();
    s.push_str(&format!("  auto captions ({} languages), English keys: {}\n", auto.len(), if shown.is_empty() { "none".to_string() } else { shown.join(", ") }));
    s
}

/// Download a caption document with retries (YouTube answers 429 readily).
pub fn fetch_caption(agent: &ureq::Agent, url: &str) -> Result<String> {
    let mut delay = Duration::from_secs(3);
    for attempt in 1..=6 {
        match agent.get(url).call() {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let body = resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_string().unwrap_or_default();
                if (200..300).contains(&status) {
                    if body.trim().is_empty() {
                        bail!("YouTube returned an empty caption document; retry later, or pass --cookies-from-browser");
                    }
                    return Ok(body);
                }
                if status != 429 && status < 500 {
                    bail!("caption download failed: HTTP {status}");
                }
                eprintln!("[diolingo] caption download got HTTP {status}, retrying in {}s (attempt {attempt}/6)", delay.as_secs());
            }
            Err(e) => {
                if attempt == 6 {
                    return Err(e).context("caption download failed");
                }
                eprintln!("[diolingo] caption download error: {e}; retrying in {}s (attempt {attempt}/6)", delay.as_secs());
            }
        }
        sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(60));
    }
    bail!("caption download failed after repeated 429/5xx responses")
}

pub struct YtDlp {
    pub bin: String,
    /// Arguments appended to every invocation (cookies, user passthrough).
    pub common_args: Vec<String>,
}

impl YtDlp {
    pub fn check(&self) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("--version")
            .output()
            .with_context(|| format!("{} not found on PATH (install with: uv tool install yt-dlp)", self.bin))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `yt-dlp -J`. Returns the parsed info and the raw JSON (kept for `--load-info-json`).
    pub fn probe(&self, url: &str) -> Result<(Info, String)> {
        let out = Command::new(&self.bin)
            .args(["-J", "--no-playlist", "--flat-playlist"])
            .args(&self.common_args)
            .arg("--")
            .arg(url)
            .stderr(Stdio::inherit())
            .output()
            .context("running yt-dlp")?;
        if !out.status.success() {
            bail!("yt-dlp could not extract {url} ({})", out.status);
        }
        let raw = String::from_utf8(out.stdout).context("yt-dlp JSON is not UTF-8")?;
        let info: Info = serde_json::from_str(&raw).context("cannot parse yt-dlp JSON")?;
        if info.id.is_empty() {
            bail!("yt-dlp returned no video id for {url}");
        }
        Ok((info, raw))
    }

    /// Download the video into `work/<id>.mkv`. Tries the saved info JSON first
    /// (no second extraction), then falls back to a fresh extraction.
    pub fn download_video(&self, url: &str, info_json: &Path, work: &Path, id: &str, max_height: u32) -> Result<PathBuf> {
        if let Some(existing) = find_video_file(work, id) {
            eprintln!("[diolingo] video already downloaded: {}", existing.display());
            return Ok(existing);
        }
        let fmt = format!(
            "bv*[height<={h}][vcodec^=avc1]+ba[ext=m4a]/bv*[height<={h}]+ba/b[height<={h}]/bv*+ba/b",
            h = max_height
        );
        let template = work.join(format!("{id}.%(ext)s"));
        let base: Vec<String> = [
            "--no-playlist", "-f", &fmt, "--merge-output-format", "mkv", "--remux-video", "mkv",
            "--no-overwrites", "--continue", "-o", template.to_str().context("non-UTF-8 work path")?,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let status = Command::new(&self.bin)
            .args(&base)
            .args(&self.common_args)
            .arg("--load-info-json")
            .arg(info_json)
            .status()
            .context("running yt-dlp")?;
        if !status.success() {
            eprintln!("[diolingo] download from saved metadata failed; retrying with a fresh extraction");
            let status = Command::new(&self.bin)
                .args(&base)
                .args(&self.common_args)
                .arg("--")
                .arg(url)
                .status()
                .context("running yt-dlp")?;
            if !status.success() {
                bail!("yt-dlp download failed ({status})");
            }
        }
        find_video_file(work, id).context("yt-dlp finished but no video file was found in the work directory")
    }
}

fn find_video_file(work: &Path, id: &str) -> Option<PathBuf> {
    ["mkv", "mp4", "webm"].iter().map(|ext| work.join(format!("{id}.{ext}"))).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_with(manual: &[&str], auto: &[&str]) -> Info {
        let mk = |keys: &[&str]| -> SubMap {
            keys.iter()
                .map(|k| (k.to_string(), vec![SubFormat { ext: "json3".into(), url: Some(format!("http://x/{k}")), name: None }]))
                .collect()
        };
        Info { kind: None, id: "id".into(), title: String::new(), url: None, subtitles: Some(mk(manual)), automatic_captions: Some(mk(auto)), entries: None }
    }

    #[test]
    fn english_prefers_manual_then_orig() {
        let i = info_with(&["en-CA"], &["en", "en-orig"]);
        let t = pick_english(&i, None).unwrap().unwrap();
        assert!(!t.auto && t.key == "en-CA");
        let i = info_with(&[], &["en", "en-orig"]);
        let t = pick_english(&i, None).unwrap().unwrap();
        assert!(t.auto && t.key == "en-orig");
    }
}
