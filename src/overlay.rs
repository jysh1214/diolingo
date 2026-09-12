//! Layer-shell overlay (GTK4): a subtitle bar on the `overlay` layer, above
//! every window including fullscreen ones. It follows mpv's `time-pos` over
//! IPC and shows the cue for that moment; the mouse wheel changes the volume
//! and a click toggles pause. It never takes keyboard focus.

use crate::align::BiCue;
use crate::mpv;
use crate::subs::Order;
use anyhow::{Context, Result};
use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct OverlayOpts {
    pub width: i32,
    pub bottom_margin: i32,
    pub font_size: i32,
    pub font_en: String,
    pub font_zh: String,
    pub order: Order,
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
    window.set_anchor(Edge::Bottom, true);
    window.set_margin(Edge::Bottom, opts.bottom_margin);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_decorated(false);
    window.set_default_size(opts.width, -1);

    let bar = gtk::Box::new(gtk::Orientation::Vertical, 2);
    bar.add_css_class("bar");
    bar.set_size_request(opts.width, -1);
    let make_label = |class: &str| {
        let l = gtk::Label::new(None);
        l.add_css_class(class);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_justify(gtk::Justification::Center);
        l.set_halign(gtk::Align::Center);
        l.set_hexpand(true);
        l
    };
    let en = make_label("en");
    let zh = make_label("zh");
    let status = make_label("status");
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
    window.set_child(Some(&bar));

    // Wheel: volume. Click: pause/resume.
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
    click.connect_released(move |_, _, _, _| {
        if let Ok(mut s) = ctl.lock() {
            let _ = mpv::send(&mut s, &json!({ "command": ["cycle", "pause"] }));
        }
    });
    window.add_controller(click);

    // Refresh from the mirrored player state; quit when mpv is gone.
    let app_ref = app.clone();
    let window_ref = window.clone();
    let mut last: Option<(usize, bool, String)> = None;
    glib::timeout_add_local(Duration::from_millis(40), move || {
        let (time_ms, paused, volume, show_volume, alive) = {
            let s = shared.lock().unwrap();
            (s.time_ms, s.paused, s.volume, s.volume_until.is_some_and(|t| Instant::now() < t), s.alive)
        };
        if !alive {
            app_ref.quit();
            return glib::ControlFlow::Break;
        }
        let idx = time_ms.and_then(|t| current_cue(&cues, t));
        let status_text = match (paused, show_volume, volume) {
            (true, _, _) => "⏸ paused".to_string(),
            (false, true, Some(v)) => format!("volume {:.0}%", v),
            _ => String::new(),
        };
        let key = (idx.unwrap_or(usize::MAX), paused, status_text.clone());
        if last.as_ref() == Some(&key) {
            return glib::ControlFlow::Continue;
        }
        last = Some(key);
        match idx {
            Some(i) => {
                en.set_text(&cues[i].en);
                zh.set_text(&cues[i].zh);
                en.set_visible(!cues[i].en.is_empty());
                zh.set_visible(!cues[i].zh.is_empty());
            }
            None => {
                en.set_visible(false);
                zh.set_visible(false);
            }
        }
        status.set_text(&status_text);
        status.set_visible(!status_text.is_empty());
        window_ref.set_visible(idx.is_some() || !status_text.is_empty());
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
