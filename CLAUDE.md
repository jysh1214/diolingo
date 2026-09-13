# diolingo — notes for Claude Code

Rust CLI: `diolingo URL` fetches a YouTube video, its English captions and a
Qwen draft of the Chinese subtitles into `~/.diolingo/[<id>] <title>/`;
`diolingo burn <id>` builds the bilingual files; `diolingo play <id>` plays the
audio with a subtitle overlay. See README.md for the full command set.

## Retranslating a video's Chinese subtitles

The owner prefers Claude's translation over the Qwen draft and asks for it in
this session (no API). Procedure when asked to translate video `<id>`:

1. Read `~/.diolingo/glossary.md` (source in this repo: `glossary.md`) and
   follow it: terms kept in English, Taiwan-style spoken Traditional Chinese,
   full-width punctuation, spaces around English/numbers, no notes.
2. Find the folder `~/.diolingo/[<id>] *` and read `<Title> [<id>].en.srt`
   in full before translating anything, so terminology stays consistent.
3. Translate every cue one-to-one. Never merge, split or complete fragments;
   the cue count and timings must stay identical.
4. Write the result with a script, not by hand: build `<Title> [<id>].zh.srt`
   from the `.en.srt` timings and a list of translations, and assert the
   counts match. Keep a backup of the previous `.zh.srt` as `.zh.srt.qwen`
   the first time.
5. Do not run `burn` unless asked; `play` picks the new `.zh.srt` up directly.
6. Report the cue count and anything left in English on purpose.

## Conventions

- Reply in zh-TW when the owner writes Chinese; technical terms stay English.
- Never commit media or subtitle outputs (`.gitignore` covers them).
- Any GPU run (Qwen) needs the owner's go-ahead first.
