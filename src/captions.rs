//! Caption parsing: YouTube `json3` (preferred) and WebVTT/SRT (fallback).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One subtitle cue. `text` holds one or more lines joined with `'\n'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Deserialize)]
struct Json3 {
    #[serde(default)]
    events: Vec<Json3Event>,
}

#[derive(Deserialize)]
struct Json3Event {
    #[serde(rename = "tStartMs", default)]
    start_ms: f64,
    #[serde(rename = "dDurationMs", default)]
    duration_ms: f64,
    /// `1` marks a synthetic "\n" event used by the rolling ASR display; skip those.
    #[serde(rename = "aAppend", default)]
    append: u8,
    #[serde(default)]
    segs: Vec<Json3Seg>,
}

#[derive(Deserialize)]
struct Json3Seg {
    #[serde(default)]
    utf8: String,
}

/// Parse a YouTube timedtext `json3` document. Works for manual subtitles,
/// auto-generated captions and auto-translated captions.
pub fn parse_json3(text: &str) -> Result<Vec<Cue>> {
    let doc: Json3 = serde_json::from_str(text).context("caption file is not valid json3")?;
    let mut cues = Vec::with_capacity(doc.events.len());
    for ev in doc.events {
        if ev.append == 1 || ev.segs.is_empty() {
            continue;
        }
        let raw: String = ev.segs.iter().map(|s| s.utf8.as_str()).collect();
        let text = clean_text(&raw);
        if text.is_empty() {
            continue;
        }
        let start_ms = ev.start_ms.max(0.0).round() as u64;
        let end_ms = start_ms + ev.duration_ms.max(0.0).round() as u64;
        cues.push(Cue { start_ms, end_ms, text });
    }
    Ok(normalize(cues))
}

/// Parse WebVTT. Handles YouTube's rolling auto-caption VTT, where each cue
/// repeats the previous line, by dropping already-emitted leading lines.
pub fn parse_vtt(text: &str) -> Result<Vec<Cue>> {
    parse_blocks(text, true)
}

/// Parse plain SRT (no rolling-line dedupe, so repeated cues survive).
pub fn parse_srt(text: &str) -> Result<Vec<Cue>> {
    parse_blocks(text, false)
}

fn parse_blocks(text: &str, dedupe_rolling: bool) -> Result<Vec<Cue>> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut cues = Vec::new();
    let mut prev_last_line: Option<String> = None;
    for block in text.split("\n\n") {
        let mut timing: Option<&str> = None;
        let mut body: Vec<&str> = Vec::new();
        for line in block.lines() {
            match timing {
                None => {
                    if line.contains("-->") {
                        timing = Some(line);
                    }
                }
                Some(_) => body.push(line),
            }
        }
        let Some(timing) = timing else { continue };
        let (a, b) = timing.split_once("-->").unwrap();
        let start_ms = parse_timestamp(a.trim())?;
        let end_ms = parse_timestamp(b.split_whitespace().next().unwrap_or(""))?;
        let cleaned = clean_text(&strip_tags(&body.join("\n")));
        if cleaned.is_empty() {
            continue;
        }
        let mut lines: Vec<&str> = cleaned.split('\n').collect();
        if dedupe_rolling
            && let Some(prev) = &prev_last_line
            && lines.first() == Some(&prev.as_str())
        {
            lines.remove(0);
        }
        if lines.is_empty() {
            continue;
        }
        prev_last_line = Some(lines.last().unwrap().to_string());
        cues.push(Cue { start_ms, end_ms, text: lines.join("\n") });
    }
    Ok(normalize(cues))
}

/// Parse by container type: `json3`, otherwise VTT/SRT.
pub fn parse(ext: &str, text: &str) -> Result<Vec<Cue>> {
    match ext {
        "json3" => parse_json3(text),
        "vtt" => parse_vtt(text),
        "srt" => parse_srt(text),
        other => bail!("unsupported caption format {other:?}"),
    }
}

/// Sort by start, merge cues that share a start time, and clamp every cue so it
/// ends no later than the next one starts (YouTube ASR lines overlap by design).
pub fn normalize(mut cues: Vec<Cue>) -> Vec<Cue> {
    cues.retain(|c| !c.text.is_empty());
    cues.sort_by_key(|c| c.start_ms);
    let mut out: Vec<Cue> = Vec::with_capacity(cues.len());
    for c in cues {
        if let Some(last) = out.last_mut() {
            if c.start_ms <= last.start_ms {
                if last.text != c.text {
                    last.text.push('\n');
                    last.text.push_str(&c.text);
                }
                last.end_ms = last.end_ms.max(c.end_ms);
                continue;
            }
            if last.end_ms > c.start_ms {
                last.end_ms = c.start_ms;
            }
        }
        out.push(c);
    }
    out.retain(|c| c.end_ms > c.start_ms);
    out
}

/// Unescape HTML entities, collapse whitespace per line, drop empty lines.
pub fn clean_text(raw: &str) -> String {
    let unescaped = html_unescape(raw);
    let mut lines: Vec<String> = Vec::new();
    for line in unescaped.split('\n') {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            lines.push(collapsed);
        }
    }
    lines.join("\n")
}

fn html_unescape(s: &str) -> String {
    let s = s.replace('\u{200b}', "");
    if !s.contains('&') {
        return s;
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// `HH:MM:SS.mmm`, `MM:SS.mmm`, or the SRT comma variant.
fn parse_timestamp(s: &str) -> Result<u64> {
    let s = s.replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, s] => (h.parse::<u64>()?, m.parse::<u64>()?, s.parse::<f64>()?),
        [m, s] => (0, m.parse::<u64>()?, s.parse::<f64>()?),
        _ => bail!("bad timestamp {s:?}"),
    };
    Ok(h * 3_600_000 + m * 60_000 + (sec * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json3_skips_append_events_and_clamps() {
        let doc = r#"{"events":[
            {"tStartMs":0,"dDurationMs":1000,"id":1},
            {"tStartMs":160,"dDurationMs":4000,"segs":[{"utf8":"rust"},{"utf8":" a","tOffsetMs":680}]},
            {"tStartMs":2669,"dDurationMs":1491,"aAppend":1,"segs":[{"utf8":"\n"}]},
            {"tStartMs":2679,"dDurationMs":3600,"segs":[{"utf8":"language &amp; that"}]}
        ]}"#;
        let cues = parse_json3(doc).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "rust a");
        assert_eq!(cues[0].end_ms, 2679);
        assert_eq!(cues[1].text, "language & that");
        assert_eq!(cues[1].end_ms, 2679 + 3600);
    }

    #[test]
    fn vtt_dedupes_rolling_lines() {
        let vtt = "WEBVTT\nKind: captions\n\n00:00:00.160 --> 00:00:02.669 align:start\n \nrust<00:00:00.840><c> a</c>\n\n00:00:02.669 --> 00:00:02.679\nrust a\n \n\n00:00:02.679 --> 00:00:04.150\nrust a\nlanguage<00:00:03.000><c> that</c>\n";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "rust a");
        assert_eq!(cues[1].text, "language that");
        assert_eq!(cues[1].start_ms, 2679);
    }

    #[test]
    fn normalize_merges_same_start() {
        let cues = vec![
            Cue { start_ms: 10, end_ms: 20, text: "a".into() },
            Cue { start_ms: 10, end_ms: 30, text: "b".into() },
            Cue { start_ms: 25, end_ms: 40, text: "c".into() },
        ];
        let n = normalize(cues);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].text, "a\nb");
        assert_eq!(n[0].end_ms, 25);
    }
}
