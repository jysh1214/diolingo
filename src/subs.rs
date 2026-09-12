//! Subtitle writers: SRT (plain) and ASS (styled, two named styles).

use crate::align::BiCue;
use crate::captions::Cue;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Which language goes on the top line of each bilingual cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Order {
    EnZh,
    ZhEn,
}

pub struct AssStyle<'a> {
    pub font_en: &'a str,
    pub font_zh: &'a str,
    /// Canvas the sizes below refer to (`PlayResX`/`PlayResY`).
    pub play_res: (u32, u32),
    pub size_en: u32,
    pub size_zh: u32,
    pub margin_h: u32,
    pub margin_v: u32,
}

impl<'a> AssStyle<'a> {
    /// Style for a full video frame (1080p canvas; libass scales it to the real size).
    pub fn video(font_en: &'a str, font_zh: &'a str) -> Self {
        Self { font_en, font_zh, play_res: (1920, 1080), size_en: 54, size_zh: 48, margin_h: 80, margin_v: 42 }
    }

    /// Style for a subtitle-only window of `width`x`height` pixels: the canvas
    /// equals the window, so the sizes are absolute pixels.
    pub fn player(font_en: &'a str, font_zh: &'a str, width: u32, height: u32) -> Self {
        let h = height as f32;
        Self {
            font_en,
            font_zh,
            play_res: (width, height),
            size_en: (h * 0.20).round() as u32,
            size_zh: (h * 0.18).round() as u32,
            margin_h: 20,
            margin_v: (h * 0.06).round() as u32,
        }
    }
}

pub fn srt_time(ms: u64) -> String {
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

pub fn ass_time(ms: u64) -> String {
    let cs = (ms + 5) / 10;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, (cs / 6000) % 60, (cs / 100) % 60, cs % 100)
}

fn write_srt_entries<'a>(path: &Path, entries: impl Iterator<Item = (u64, u64, &'a str)>) -> Result<()> {
    let mut out = String::new();
    let mut n = 0;
    for (start, end, text) in entries {
        if text.is_empty() {
            continue;
        }
        n += 1;
        let _ = write!(out, "{n}\n{} --> {}\n{text}\n\n", srt_time(start), srt_time(end));
    }
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

/// Bilingual SRT: one cue with the two languages on separate lines.
pub fn write_bilingual_srt(path: &Path, cues: &[BiCue], order: Order) -> Result<()> {
    let joined: Vec<(u64, u64, String)> = cues
        .iter()
        .map(|c| {
            let text = match (c.en.is_empty(), c.zh.is_empty(), order) {
                (true, _, _) => c.zh.clone(),
                (_, true, _) => c.en.clone(),
                (false, false, Order::EnZh) => format!("{}\n{}", c.en, c.zh),
                (false, false, Order::ZhEn) => format!("{}\n{}", c.zh, c.en),
            };
            (c.start_ms, c.end_ms, text)
        })
        .collect();
    write_srt_entries(path, joined.iter().map(|(s, e, t)| (*s, *e, t.as_str())))
}

/// Single-language SRT from plain cues.
pub fn write_srt(path: &Path, cues: &[Cue]) -> Result<()> {
    write_srt_entries(path, cues.iter().map(|c| (c.start_ms, c.end_ms, c.text.as_str())))
}

/// Chinese-only SRT taken from the bilingual cues.
pub fn write_zh_srt(path: &Path, cues: &[BiCue]) -> Result<()> {
    write_srt_entries(path, cues.iter().map(|c| (c.start_ms, c.end_ms, c.zh.as_str())))
}

fn ass_escape(text: &str) -> String {
    text.replace('{', "｛").replace('}', "｝").replace('\n', "\\N")
}

/// Styled bilingual ASS. English uses style `EN`, Chinese uses style `ZH`;
/// the second line of each cue switches style with `{\rZH}` / `{\rEN}`.
pub fn write_ass(path: &Path, cues: &[BiCue], order: Order, style: &AssStyle) -> Result<()> {
    let mut out = String::new();
    let _ = write!(out, "[Script Info]\nScriptType: v4.00+\nPlayResX: {}\nPlayResY: {}\nWrapStyle: 0\nScaledBorderAndShadow: yes\nYCbCr Matrix: None\n\n", style.play_res.0, style.play_res.1);
    out.push_str("[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n");
    let (mh, mv) = (style.margin_h, style.margin_v);
    let _ = writeln!(out, "Style: EN,{},{},&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2.5,1,2,{mh},{mh},{mv},1", style.font_en, style.size_en);
    let _ = writeln!(out, "Style: ZH,{},{},&H0096E6FF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2.5,1,2,{mh},{mh},{mv},1", style.font_zh, style.size_zh);
    out.push_str("\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    for c in cues {
        let en = ass_escape(&c.en);
        let zh = ass_escape(&c.zh);
        let (style_name, text) = match (en.is_empty(), zh.is_empty(), order) {
            (true, true, _) => continue,
            (false, true, _) => ("EN", en),
            (true, false, _) => ("ZH", zh),
            (false, false, Order::EnZh) => ("EN", format!("{en}\\N{{\\rZH}}{zh}")),
            (false, false, Order::ZhEn) => ("ZH", format!("{zh}\\N{{\\rEN}}{en}")),
        };
        let _ = writeln!(out, "Dialogue: 0,{},{},{style_name},,0,0,0,,{text}", ass_time(c.start_ms), ass_time(c.end_ms));
    }
    fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formats() {
        assert_eq!(srt_time(3_723_456), "01:02:03,456");
        assert_eq!(ass_time(3_723_456), "1:02:03.46");
        assert_eq!(ass_time(0), "0:00:00.00");
    }
}
