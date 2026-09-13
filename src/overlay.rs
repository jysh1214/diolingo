//! Layer-shell overlay (GTK4): a transparent, monitor-sized surface on the
//! `overlay` layer (above every window, fullscreen ones included) that draws
//! the subtitle bar at a chosen spot. The surface itself never moves, so
//! pointer coordinates are screen coordinates and dragging the bar is exact;
//! the input region is limited to the bar, so everything else stays
//! click-through. The bar follows mpv's `time-pos` over IPC; the wheel changes
//! the volume, a click toggles pause. It never takes keyboard focus.

use crate::align::{self, BiCue};
use crate::mpv;
use crate::subs::Order;
use anyhow::{Context, Result};
use gtk4 as gtk;
use gtk4::cairo;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde_json::{Value, json};
use std::cell::Cell;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct OverlayOpts {
    pub width: i32,
    pub font_size: i32,
    pub font_en: String,
    pub font_zh: String,
    pub order: Order,
    /// Where the bar sits; dragging it updates and saves this.
    pub position: Position,
    /// File the position is saved to after a drag.
    pub position_file: PathBuf,
    /// Shadowing: pause after every sentence so it can be repeated.
    pub shadow: Option<ShadowOpts>,
}

#[derive(Debug, Clone, Copy)]
pub struct ShadowOpts {
    /// Pause length as a multiple of the sentence's own duration.
    pub ratio: f64,
    /// Longest sentence to build from consecutive cues, in ms.
    pub chunk_ms: u64,
}

/// A sentence: consecutive cues `first..=last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chunk {
    first: usize,
    last: usize,
    start_ms: u64,
    end_ms: u64,
}

/// Group cues into sentences: a chunk ends at terminal punctuation, before a
/// gap of `GAP_MS` or more, or once it is `max_ms` long.
fn chunk_cues(cues: &[BiCue], max_ms: u64) -> Vec<Chunk> {
    const GAP_MS: u64 = 700;
    let mut out = Vec::new();
    let mut i = 0;
    while i < cues.len() {
        let first = i;
        let start_ms = cues[i].start_ms;
        loop {
            let c = &cues[i];
            let text = c.en.trim_end().trim_end_matches(['"', '\'', ')', ']', '”', '』', '」']);
            let sentence_end = text.ends_with(['.', '?', '!', '。', '？', '！']);
            let last = i + 1 >= cues.len()
                || sentence_end
                || cues[i + 1].start_ms.saturating_sub(c.end_ms) >= GAP_MS
                || c.end_ms.saturating_sub(start_ms) >= max_ms;
            if last {
                break;
            }
            i += 1;
        }
        out.push(Chunk { first, last: i, start_ms, end_ms: cues[i].end_ms });
        i += 1;
    }
    out
}

fn chunk_at(chunks: &[Chunk], t_ms: u64) -> Option<usize> {
    let i = chunks.partition_point(|c| c.start_ms <= t_ms).checked_sub(1)?;
    (t_ms < chunks[i].end_ms).then_some(i)
}

/// How long to pause after a sentence of `len_ms`.
fn shadow_pause(len_ms: u64, ratio: f64) -> Duration {
    let secs = (len_ms as f64 / 1000.0 * ratio + 0.5).clamp(1.5, 12.0);
    Duration::from_secs_f64(secs)
}

fn set_pause(control: &Arc<Mutex<UnixStream>>, paused: bool) {
    if let Ok(mut s) = control.lock() {
        let _ = mpv::send(&mut s, &json!({ "command": ["set_property", "pause", paused] }));
    }
}

/// Bar position in screen pixels: distance from the left edge (`None` keeps
/// it centred) and from the bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub left: Option<i32>,
    pub bottom: i32,
}

impl Position {
    pub const DEFAULT: Position = Position { left: None, bottom: 40 };

    /// Parse the saved `left=<px>` / `bottom=<px>` lines; missing file or
    /// garbage yields `None`.
    pub fn load(path: &Path) -> Option<Position> {
        let text = fs::read_to_string(path).ok()?;
        let mut pos = Position::DEFAULT;
        for line in text.lines() {
            match line.trim().split_once('=') {
                Some(("left", v)) => pos.left = v.trim().parse().ok(),
                Some(("bottom", v)) => pos.bottom = v.trim().parse().ok()?,
                _ => {}
            }
        }
        Some(pos)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut text = format!("bottom={}\n", self.bottom);
        if let Some(l) = self.left {
            text.push_str(&format!("left={l}\n"));
        }
        fs::write(path, text)
    }
}

/// Player state mirrored from mpv property-change events.
#[derive(Default)]
struct Shared {
    time_ms: Option<u64>,
    paused: bool,
    volume: Option<f64>,
    /// Show the volume readout until this instant (set on volume changes).
    volume_until: Option<Instant>,
    /// False once mpv closed the socket.
    alive: bool,
}

pub fn run(cues: Vec<BiCue>, opts: OverlayOpts, socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?;
    let mut control = stream.try_clone()?;
    for (id, prop) in [(1, "time-pos"), (2, "pause"), (3, "volume")] {
        mpv::send(&mut control, &json!({ "command": ["observe_property", id, prop] }))?;
    }
    let shared = Arc::new(Mutex::new(Shared { alive: true, ..Default::default() }));
    spawn_reader(stream, shared.clone());

    let app = gtk::Application::builder().flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
    let cues = Rc::new(cues);
    let opts = Rc::new(opts);
    let control = Arc::new(Mutex::new(control));
    app.connect_activate(move |app| build_ui(app, cues.clone(), opts.clone(), shared.clone(), control.clone()));
    // GTK must not parse diolingo's own arguments.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn spawn_reader(stream: UnixStream, shared: Arc<Mutex<Shared>>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("event").and_then(Value::as_str) != Some("property-change") {
                continue;
            }
            let mut s = shared.lock().unwrap();
            match msg.get("name").and_then(Value::as_str) {
                Some("time-pos") => s.time_ms = msg.get("data").and_then(Value::as_f64).map(|t| (t.max(0.0) * 1000.0) as u64),
                Some("pause") => s.paused = msg.get("data").and_then(Value::as_bool).unwrap_or(false),
                Some("volume") => {
                    let v = msg.get("data").and_then(Value::as_f64);
                    if s.volume.is_some() && v != s.volume {
                        s.volume_until = Some(Instant::now() + Duration::from_millis(1500));
                    }
                    s.volume = v;
                }
                _ => {}
            }
        }
        shared.lock().unwrap().alive = false;
    });
}

/// Where the bar was last placed, to skip redundant GTK/GDK calls.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Placement {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    visible: bool,
}

/// Places the bar inside the monitor-sized surface and keeps the surface's
/// input region equal to the bar.
struct Layout {
    window: gtk::ApplicationWindow,
    fixed: gtk::Fixed,
    bar: gtk::Box,
    width: i32,
    last: Cell<Option<Placement>>,
}

impl Layout {
    /// Logical size of the surface (= the monitor once mapped).
    fn screen(&self) -> (i32, i32) {
        let (w, h) = (self.window.width(), self.window.height());
        if w > 0 && h > 0 {
            return (w, h);
        }
        gtk::gdk::Display::default()
            .and_then(|d| d.monitors().item(0).and_downcast::<gtk::gdk::Monitor>())
            .map(|m| (m.geometry().width(), m.geometry().height()))
            .unwrap_or((1920, 1080))
    }

    fn bar_size(&self) -> (i32, i32) {
        let (_, natural_h, _, _) = self.bar.measure(gtk::Orientation::Vertical, self.width);
        (self.width, natural_h.max(1))
    }

    /// The bar's left edge for `pos` (centred when `pos.left` is `None`).
    fn left_of(&self, pos: Position) -> i32 {
        let (sw, _) = self.screen();
        pos.left.unwrap_or((sw - self.width) / 2).clamp(0, (sw - self.width).max(0))
    }

    fn place(&self, pos: Position, visible: bool) {
        let (_, sh) = self.screen();
        let (bw, bh) = self.bar_size();
        let x = self.left_of(pos);
        let y = (sh - pos.bottom - bh).max(0);
        let placement = Placement { x, y, w: bw, h: bh, visible };
        if self.last.get() == Some(placement) {
            return;
        }
        self.last.set(Some(placement));
        self.fixed.move_(&self.bar, f64::from(x), f64::from(y));
        if let Some(surface) = self.window.surface() {
            let region = if visible {
                cairo::Region::create_rectangle(&cairo::RectangleInt::new(x, y, bw, bh))
            } else {
                cairo::Region::create()
            };
            surface.set_input_region(Some(&region));
        }
    }
}

fn build_ui(app: &gtk::Application, cues: Rc<Vec<BiCue>>, opts: Rc<OverlayOpts>, shared: Arc<Mutex<Shared>>, control: Arc<Mutex<UnixStream>>) {
    let css = gtk::CssProvider::new();
    css.load_from_string(&stylesheet(&opts));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = gtk::ApplicationWindow::new(app);
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("diolingo"));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_decorated(false);

    let fixed = gtk::Fixed::new();
    let bar = gtk::Box::new(gtk::Orientation::Vertical, 2);
    bar.add_css_class("bar");
    bar.set_size_request(opts.width, -1);
    bar.set_visible(false);
    // Inside a GtkFixed a label would otherwise grow to its unwrapped width.
    let usable = opts.width - 56;
    let make_label = |class: &str, px: i32| {
        let l = gtk::Label::new(None);
        l.add_css_class(class);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_justify(gtk::Justification::Center);
        l.set_halign(gtk::Align::Center);
        l.set_hexpand(true);
        l.set_max_width_chars((f64::from(usable) / (f64::from(px) * 0.6)).max(10.0) as i32);
        l
    };
    let en = make_label("en", opts.font_size);
    let zh = make_label("zh", (opts.font_size as f32 * 0.9).round() as i32);
    let status = make_label("status", opts.font_size / 2);
    match opts.order {
        Order::EnZh => {
            bar.append(&en);
            bar.append(&zh);
        }
        Order::ZhEn => {
            bar.append(&zh);
            bar.append(&en);
        }
    }
    bar.append(&status);
    fixed.put(&bar, 0.0, 0.0);
    window.set_child(Some(&fixed));

    let position = Rc::new(Cell::new(opts.position));
    let layout = Rc::new(Layout { window: window.clone(), fixed, bar: bar.clone(), width: opts.width, last: Cell::new(None) });

    // Drag: move the bar. The gesture sits on the window, whose coordinates are
    // the monitor's, so the offsets are exact however the bar moves.
    let drag = gtk::GestureDrag::new();
    let start = Rc::new(Cell::new(Position::DEFAULT));
    let dragged = Rc::new(Cell::new(false));
    {
        let (l, p, s, d) = (layout.clone(), position.clone(), start.clone(), dragged.clone());
        drag.connect_drag_begin(move |_, _, _| {
            s.set(Position { left: Some(l.left_of(p.get())), bottom: p.get().bottom });
            d.set(false);
        });
    }
    {
        let (l, p, s, d) = (layout.clone(), position.clone(), start.clone(), dragged.clone());
        drag.connect_drag_update(move |_, ox, oy| {
            if !d.get() && ox.abs() < 3.0 && oy.abs() < 3.0 {
                return;
            }
            d.set(true);
            let st = s.get();
            let (sw, sh) = l.screen();
            let (bw, bh) = l.bar_size();
            let next = Position {
                left: Some((st.left.unwrap_or(0) + ox.round() as i32).clamp(0, (sw - bw).max(0))),
                bottom: (st.bottom - oy.round() as i32).clamp(0, (sh - bh).max(0)),
            };
            if next != p.get() {
                p.set(next);
                l.place(next, true);
            }
        });
    }
    {
        let (p, d, file) = (position.clone(), dragged.clone(), opts.position_file.clone());
        drag.connect_drag_end(move |_, _, _| {
            if d.get()
                && let Err(e) = p.get().save(&file)
            {
                eprintln!("[diolingo] could not save the bar position: {e}");
            }
        });
    }
    window.add_controller(drag);

    // Wheel: volume. Click (without dragging): pause/resume.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let ctl = control.clone();
    scroll.connect_scroll(move |_, _dx, dy| {
        let step = if dy < 0.0 { 5 } else { -5 };
        if let Ok(mut s) = ctl.lock() {
            let _ = mpv::send(&mut s, &json!({ "command": ["add", "volume", step] }));
        }
        glib::Propagation::Stop
    });
    window.add_controller(scroll);
    let click = gtk::GestureClick::new();
    let ctl = control.clone();
    let dragged_ref = dragged.clone();
    click.connect_released(move |_, _, _, _| {
        if dragged_ref.get() {
            return;
        }
        if let Ok(mut s) = ctl.lock() {
            let _ = mpv::send(&mut s, &json!({ "command": ["cycle", "pause"] }));
        }
    });
    window.add_controller(click);

    // Refresh from the mirrored player state; quit when mpv is gone.
    let app_ref = app.clone();
    let shadow_opts = opts.shadow;
    let chunks = shadow_opts.map(|o| chunk_cues(&cues, o.chunk_ms)).unwrap_or_default();
    let control_ref = control.clone();
    let mut last: Option<(String, String, String)> = None;
    // Shadowing state: the sentence the playhead was last inside, the one we
    // already paused for, and the pause in progress (sentence, deadline,
    // whether mpv has confirmed the pause).
    let mut seen_chunk: Option<usize> = None;
    let mut shadowed: Option<usize> = None;
    let mut pausing: Option<(usize, Instant, bool)> = None;
    glib::timeout_add_local(Duration::from_millis(40), move || {
        let (time_ms, paused, volume, show_volume, alive) = {
            let s = shared.lock().unwrap();
            (s.time_ms, s.paused, s.volume, s.volume_until.is_some_and(|t| Instant::now() < t), s.alive)
        };
        if !alive {
            app_ref.quit();
            return glib::ControlFlow::Break;
        }

        // Shadowing: pause at the end of each sentence, resume after the countdown.
        let mut shadow_text: Option<(String, String, String)> = None;
        if let (Some(o), Some(t)) = (shadow_opts, time_ms) {
            if let Some((k, until, confirmed)) = pausing {
                let now = Instant::now();
                let confirmed = confirmed || paused;
                if confirmed && (!paused || now >= until) {
                    // Countdown over, or the listener resumed early.
                    if paused {
                        set_pause(&control_ref, false);
                    }
                    pausing = None;
                    shadowed = Some(k);
                } else {
                    pausing = Some((k, until, confirmed));
                    let c = &chunks[k];
                    let en_all = cues[c.first..=c.last].iter().map(|x| x.en.replace('\n', " ")).collect::<Vec<_>>().join(" ");
                    let zh_all = align::flatten_zh(&cues[c.first..=c.last].iter().map(|x| x.zh.as_str()).collect::<Vec<_>>().join("\n"));
                    let left = until.saturating_duration_since(now).as_secs_f64();
                    shadow_text = Some((en_all, zh_all, format!("repeat  {left:.1} s")));
                }
            } else {
                let cur = chunk_at(&chunks, t);
                if let Some(k) = shadowed
                    && t < chunks[k].start_ms
                {
                    shadowed = None; // seeked back: this sentence may be practised again
                }
                if let Some(k) = seen_chunk
                    && shadowed != Some(k)
                    && !paused
                    && t + 30 >= chunks[k].end_ms
                    && t < chunks[k].end_ms + 500
                {
                    set_pause(&control_ref, true);
                    let c = &chunks[k];
                    pausing = Some((k, Instant::now() + shadow_pause(c.end_ms - c.start_ms, o.ratio), false));
                }
                if cur.is_some() {
                    seen_chunk = cur;
                }
            }
        }

        let idx = time_ms.and_then(|t| current_cue(&cues, t));
        let (en_text, zh_text, status_text) = match shadow_text {
            Some(t) => t,
            None => {
                let status_text = match (paused, show_volume, volume) {
                    (true, _, _) => "⏸ paused".to_string(),
                    (false, true, Some(v)) => format!("volume {:.0}%", v),
                    _ => String::new(),
                };
                match idx {
                    Some(i) => (cues[i].en.clone(), cues[i].zh.clone(), status_text),
                    None => (String::new(), String::new(), status_text),
                }
            }
        };
        let key = (en_text, zh_text, status_text);
        if last.as_ref() != Some(&key) {
            let (en_text, zh_text, status_text) = &key;
            en.set_text(en_text);
            zh.set_text(zh_text);
            en.set_visible(!en_text.is_empty());
            zh.set_visible(!zh_text.is_empty());
            status.set_text(status_text);
            status.set_visible(!status_text.is_empty());
            bar.set_visible(!en_text.is_empty() || !zh_text.is_empty() || !status_text.is_empty());
            last = Some(key);
        }
        layout.place(position.get(), bar.is_visible());
        glib::ControlFlow::Continue
    });

    // Ctrl-C in the terminal reaches mpv as well (same process group); its exit
    // closes the socket, which ends the loop above.
    window.present();
}

/// Index of the cue covering `t_ms`, if any (cues are sorted, non-overlapping).
fn current_cue(cues: &[BiCue], t_ms: u64) -> Option<usize> {
    let i = cues.partition_point(|c| c.start_ms <= t_ms).checked_sub(1)?;
    (t_ms < cues[i].end_ms).then_some(i)
}

fn stylesheet(o: &OverlayOpts) -> String {
    let zh_size = (o.font_size as f32 * 0.9).round() as i32;
    let small = (o.font_size as f32 * 0.5).round().max(12.0) as i32;
    format!(
        "window {{ background-color: transparent; }}\n\
         .bar {{ background-color: rgba(0, 0, 0, 0.62); border-radius: 14px; padding: 10px 28px; }}\n\
         .en {{ color: #ffffff; font-family: \"{}\"; font-size: {}px; text-shadow: 0 0 3px #000000; }}\n\
         .zh {{ color: #ffe696; font-family: \"{}\"; font-size: {}px; text-shadow: 0 0 3px #000000; }}\n\
         .status {{ color: #bbbbbb; font-size: {}px; }}\n",
        o.font_en, o.font_size, o.font_zh, zh_size, small
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_round_trip() {
        let path = std::env::temp_dir().join(format!("diolingo-pos-{}", std::process::id()));
        Position { left: Some(120), bottom: 300 }.save(&path).unwrap();
        assert_eq!(Position::load(&path), Some(Position { left: Some(120), bottom: 300 }));
        Position { left: None, bottom: 40 }.save(&path).unwrap();
        assert_eq!(Position::load(&path), Some(Position::DEFAULT));
        fs::remove_file(&path).unwrap();
        assert_eq!(Position::load(&path), None);
    }

    #[test]
    fn chunks_follow_punctuation_gaps_and_length() {
        let c = |s: u64, e: u64, en: &str| BiCue { start_ms: s, end_ms: e, en: en.into(), zh: String::new() };
        let cues = vec![
            c(0, 1000, "hello there"),
            c(1000, 2000, "how are you?"),   // sentence end
            c(2000, 3000, "fine"),
            c(4000, 5000, "and you"),        // gap of 1000 before this one
            c(5000, 9000, "a very long one"),
            c(9000, 10000, "continues"),     // previous chunk hit the 5 s cap at 9000
        ];
        let ch = chunk_cues(&cues, 5000);
        let spans: Vec<(usize, usize)> = ch.iter().map(|k| (k.first, k.last)).collect();
        assert_eq!(spans, vec![(0, 1), (2, 2), (3, 4), (5, 5)]);
        assert_eq!(chunk_at(&ch, 1500), Some(0));
        assert_eq!(chunk_at(&ch, 3500), None);
        assert_eq!(shadow_pause(3000, 1.0), Duration::from_secs_f64(3.5));
        assert_eq!(shadow_pause(200, 1.0), Duration::from_secs_f64(1.5));
        assert_eq!(shadow_pause(60_000, 1.0), Duration::from_secs_f64(12.0));
    }

    #[test]
    fn cue_lookup() {
        let cues = vec![
            BiCue { start_ms: 0, end_ms: 1000, en: "a".into(), zh: String::new() },
            BiCue { start_ms: 1500, end_ms: 2000, en: "b".into(), zh: String::new() },
        ];
        assert_eq!(current_cue(&cues, 0), Some(0));
        assert_eq!(current_cue(&cues, 999), Some(0));
        assert_eq!(current_cue(&cues, 1200), None);
        assert_eq!(current_cue(&cues, 1500), Some(1));
        assert_eq!(current_cue(&cues, 2000), None);
    }
}
