//! Pair English cues with their translations into bilingual cues.

use crate::captions::Cue;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BiCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub en: String,
    pub zh: String,
}

/// Build bilingual cues from an English list and a 1:1 list of translations.
pub fn zip(en: &[Cue], zh: &[String]) -> Vec<BiCue> {
    en.iter()
        .zip(zh)
        .map(|(c, t)| BiCue { start_ms: c.start_ms, end_ms: c.end_ms, en: c.text.clone(), zh: flatten_zh(t) })
        .collect()
}

/// Collapse a multi-line Chinese cue into one line.
pub fn flatten_zh(text: &str) -> String {
    let mut acc = String::new();
    for line in text.split('\n') {
        join_piece(&mut acc, line.trim());
    }
    acc
}

/// Append `piece`, inserting a space only between non-CJK neighbours.
fn join_piece(acc: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if let (Some(last), Some(first)) = (acc.chars().last(), piece.chars().next())
        && !is_cjk(last)
        && !is_cjk(first)
    {
        acc.push(' ');
    }
    acc.push_str(piece);
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF |
        0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_pairs_and_flattens() {
        let en = vec![Cue { start_ms: 0, end_ms: 1000, text: "a\nb".into() }];
        let bi = zip(&en, &["甲\n乙 c".to_string()]);
        assert_eq!(bi.len(), 1);
        assert_eq!(bi[0].en, "a\nb");
        assert_eq!(bi[0].zh, "甲乙 c");
    }

    #[test]
    fn spaces_only_between_latin() {
        let mut s = String::from("中文");
        join_piece(&mut s, "English");
        join_piece(&mut s, "word");
        join_piece(&mut s, "漢字");
        assert_eq!(s, "中文English word漢字");
    }
}
