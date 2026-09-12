# diolingo

Download a YouTube video and attach bilingual English + Chinese subtitles, for
personal language study. The Chinese is produced locally by a Qwen model on
your GPU.

```sh
diolingo URL
```

produces, in `~/.diolingo/[<video id>] <title>/`:

| File | Content |
|------|---------|
| `<Title> [id].mkv` | video + 4 subtitle tracks: styled EN+ZH (default), plain EN+ZH, EN only, ZH only |
| `<Title> [id].hardsub.mkv` | the same video with the styled EN+ZH subtitles burned into the picture (`--no-burn` skips it) |
| `<Title> [id].m4a` | audio only, for listening practice (`--no-audio` skips it) |
| `<Title> [id].srt` | bilingual SRT (English line, then Chinese line) |
| `<Title> [id].ass` | bilingual ASS with separate `EN` / `ZH` styles (white / pale yellow) |
| `<Title> [id].en.srt`, `.zh.srt` | single-language sidecars |
| `.work/` | info JSON, raw captions, the downloaded video, cached translations |

`--out DIR` moves the whole tree to `DIR/.diolingo/[<video id>] <title>/`. A
re-run finds the folder by its `[<video id>]` prefix (so a renamed video still
lands in the same place) and reuses everything in `.work/`; pass `--clean` to
delete it afterwards, `--force` to ignore it.

## Installation

```sh
cargo install --path .
```

The binary is self-contained: the translation script and its uv lock file are
embedded and written to `~/.diolingo/.scripts/` on first run (and rewritten
whenever a newer binary carries a different version). The repository does not
need to stay around after installing.

## Requirements

- `uv` (runs the translation script and manages its Python environment).
- `yt-dlp` on PATH (`uv tool install yt-dlp`) plus a JS runtime it can use
  (`deno` on PATH; yt-dlp warns and may miss formats without one).
- `ffmpeg` built with libass (renders the hard-subbed copy).
- `mpv`, GTK 4 and `gtk4-layer-shell` for `diolingo play` (Arch:
  `sudo pacman -S mpv gtk4 gtk4-layer-shell`; the GTK libraries are also
  needed to build).
- An NVIDIA GPU. Qwen3-8B in bf16 needs about 17 GB of VRAM; see the model
  table below for smaller or quantised options.
- Fonts for the styled track: defaults are `Noto Sans` and `Noto Sans CJK TC`
  (`--font-en`, `--font-zh` to change).

## Qwen translator

`scripts/translate_qwen.py` runs a Qwen model with `transformers` on CUDA.
`diolingo` invokes the copy it wrote to `~/.diolingo/.scripts/` as
`uv run --script translate_qwen.py`; the script declares its dependencies in a
PEP 723 header (pinned by `translate_qwen.py.lock` next to it), so uv builds
and caches the Python environment by itself on the first run. Expect that
first run to download the CUDA build of torch (a few GB) and the model weights
(16 GB for Qwen3-8B, into `~/.cache/huggingface`). Nothing has to be set up by
hand. `--script PATH` runs a different copy of the script (for local edits),
and `--python PATH` runs it with a specific interpreter instead of uv.

### Model choice and tuning

Pass `--model` to change the model and `--qwen-arg` to forward options to the
script (repeatable; use the `--qwen-arg=--flag` form for values that start
with a dash).

| Model | VRAM (bf16) | Note |
|-------|-------------|------|
| `Qwen/Qwen3-8B` (default) | ~17 GB | fits a 24 GB card |
| `Qwen/Qwen3-4B` | ~9 GB | faster, lower quality |
| `Qwen/Qwen3-14B` | ~30 GB | use `--qwen-arg=--quant --qwen-arg=4bit` (~10 GB) |

Script options worth knowing (`uv run --script scripts/translate_qwen.py --help`):

- `--batch-lines N` (diolingo `--batch`, default 16): subtitle lines per
  prompt. Larger batches are faster but the model starts merging fragments
  and drifting off the line numbers (40 lost a third of the lines in testing).
- `--batch-prompts N` (default 8): prompts generated together in one forward
  batch. Lower it if you run out of VRAM (each prompt costs roughly 250 MB of
  KV cache with Qwen3-8B); raise it on bigger cards for more throughput.
- `--quant 4bit|8bit`: bitsandbytes quantisation for models that do not fit.
- `--device cpu`: run without a GPU (slow; only sensible for small models).
- `--debug`: print the raw model replies to stderr.

### What the model sees

The English cues go to the model in chunks of `--batch` numbered lines. Each
prompt carries the video title, the 3 preceding lines as context (not
translated), and the chunk itself; the model never sees the whole transcript
at once. It is asked to echo each English line next to its translation. A
reply whose echo does not match its input, or whose "translation" contains no
Chinese, is discarded and that line is retried on its own. Lines are always
translated 1:1, so the Chinese stays on the cue it belongs to.

On an RTX 4090, Qwen3-8B loads in a few seconds from disk and translates a
2.5-minute video (74 lines) in about 15 s. Translations are cached per video
in the work directory, keyed by model, script, options and the English text,
so re-running with the same settings does not re-translate.

`--zh zh-hant` (default) asks for Traditional Chinese with Taiwan usage;
`--zh zh-hans` asks for Simplified.

Examples:

```sh
# default: local Qwen3-8B on CUDA
diolingo URL

# smaller model, and fewer prompts per batch to save VRAM
diolingo --model Qwen/Qwen3-4B --qwen-arg=--batch-prompts --qwen-arg=4 URL

# see which caption tracks exist, then force the English source
diolingo --list-subs URL
diolingo --en-lang en-orig URL
```

## Listening with an always-on-top subtitle overlay

```sh
diolingo play ID
```

`play` takes the video id (the bracketed part of the folder name), finds the
folder under `~/.diolingo/` (or `--out DIR`), starts `mpv` headless on the
`.m4a`, and draws the bilingual subtitles itself in a bar at the bottom of the
screen. The bar is a layer-shell surface on the *overlay* layer, so it stays
above every window, fullscreen games included, and never takes keyboard
focus. It follows mpv's playback position over the IPC socket
(`$XDG_RUNTIME_DIR/diolingo-mpv.sock`) and disappears between cues. Starting
`play` again replaces the running player.

On the bar: drag it to move it (the position is remembered in
`~/.diolingo/.overlay-position`), mouse wheel changes the volume (±5, shown
briefly), a click pauses or resumes. Everything else goes through
`diolingo ctl`, which forwards any mpv input command to the player:

```sh
diolingo ctl sub-seek -1      # previous subtitle line
diolingo ctl sub-seek 1       # next line
diolingo ctl cycle pause
diolingo ctl add volume 5
diolingo ctl seek -10
diolingo ctl ab-loop           # set A, then B, then clear
diolingo ctl quit
```

Bind them to global keys in your compositor so they work while a game or
another window has focus (use the installed binary's absolute path if the
compositor's `PATH` does not include `~/.cargo/bin`).

Options: `--width N` (bar width, default 1600), `--left N` / `--bottom N`
(position in pixels; default is the last dragged position, else centred and
40 px up), `--reset-position` (forget the dragged position), `--font-size N`
(English size, default 40, Chinese 90% of it), `--volume N`, `--order zh-en`, `--font-en` / `--font-zh` (used as
CSS font families), `--mpv-arg=...` forwarded to mpv, `--dry-run` to print
the mpv command. Requires GTK 4 and gtk4-layer-shell at build time (Arch:
`gtk4`, `gtk4-layer-shell`) and mpv at run time.

## Other options

- `--out DIR`: base directory (default `$HOME`); files go to `DIR/.diolingo/[<video id>] <title>/`.
- `--work DIR`: keep downloads and caches in `DIR/<video id>/` instead of the
  video's `.work/`.
- `--no-video`: only write the subtitle files.
- `--no-burn`: skip the hard-subbed copy and only produce the MKV with
  switchable subtitle tracks.
- `--no-audio`: skip the audio-only `.m4a` (stream-copied when YouTube's track
  is AAC, otherwise transcoded to AAC).
- `--order zh-en`: Chinese line on top.
- `--max-height 2160`: download resolution cap (default 1080).
- `--cookies-from-browser firefox` / `--cookies FILE`: use your logged-in
  YouTube session (Premium formats, age-restricted videos).
- `--yt-dlp-arg=ARG`: passed verbatim to every yt-dlp call (repeatable).
- Playlist URLs are expanded and every entry is processed; one failure does
  not stop the rest.

## Notes

- YouTube rate-limits caption downloads (HTTP 429); the fetcher backs off and
  retries up to six times, so a run can pause for a minute.
- Personal, non-commercial use only. Respect the terms of the videos you
  download.
