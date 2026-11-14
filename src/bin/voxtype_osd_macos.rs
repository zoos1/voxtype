//! `voxtype-osd-macos` — native macOS on-screen display for voxtype.
//!
//! A borderless, click-through, non-activating panel centered on the
//! screen that has keyboard focus. Smoke-glass background (dark HUD
//! vibrancy), white text. It shows:
//!
//! - while recording: a live input-level strip and elapsed time
//! - while transcribing: "Transcribing…"
//! - when text is delivered: the transcript, briefly, then fades out
//! - when nothing was heard: "No speech detected"
//!
//! It reads only what the daemon already publishes in its runtime dir
//! (`state`, `recording.json`, `outcome.json`, `last_text`, `audio.sock`),
//! so it can crash or restart without affecting dictation. The daemon's OSD
//! supervisor launches it through the `voxtype-osd` launcher.
//!
//! Fork addition (zoos1/voxtype); not part of upstream voxtype.

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("voxtype-osd-macos {}", voxtype::cli::VERSION);
        return;
    }
    mac::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("voxtype-osd-macos only runs on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
mod mac {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::NSBitmapImageFileType;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSApplication,
        NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask, NSFont,
        NSLineBreakMode, NSPanel, NSScreen, NSTextAlignment, NSTextField,
        NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
        NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_foundation::{
        NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSRect, NSSize, NSString,
    };

    use voxtype::audio::levels::{AudioFrame, FRAME_BYTES};
    use voxtype::config::Config;

    const TICK: f64 = 1.0 / 30.0;
    const MAX_TEXT_WIDTH: f64 = 640.0;
    const PAD_X: f64 = 28.0;
    const PAD_Y: f64 = 18.0;
    const MIN_WIDTH: f64 = 240.0;
    const CORNER: f64 = 18.0;
    const LEVEL_BARS: usize = 28;
    const BAR_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const MAX_TEXT_CHARS: usize = 320;

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn read_trimmed(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn read_json(path: &Path) -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Input-level history fed from the daemon's audio socket.
    struct Levels {
        sock: PathBuf,
        stream: Option<UnixStream>,
        pending: Vec<u8>,
        peaks: Vec<f32>,
        frames_in_bar: usize,
        bar_peak: f32,
    }

    impl Levels {
        fn new(sock: PathBuf) -> Self {
            Self {
                sock,
                stream: None,
                pending: Vec::new(),
                peaks: vec![-120.0; LEVEL_BARS],
                frames_in_bar: 0,
                bar_peak: -120.0,
            }
        }

        fn reset(&mut self) {
            self.peaks.iter_mut().for_each(|p| *p = -120.0);
            self.frames_in_bar = 0;
            self.bar_peak = -120.0;
        }

        fn poll(&mut self) {
            if self.stream.is_none() {
                if let Ok(s) = UnixStream::connect(&self.sock) {
                    let _ = s.set_nonblocking(true);
                    self.stream = Some(s);
                    self.pending.clear();
                }
            }
            let Some(stream) = self.stream.as_mut() else {
                return;
            };
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        self.stream = None;
                        break;
                    }
                    Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.stream = None;
                        break;
                    }
                }
            }
            let whole = self.pending.len() / FRAME_BYTES * FRAME_BYTES;
            let frames: Vec<AudioFrame> = self.pending[..whole]
                .chunks_exact(FRAME_BYTES)
                .map(|c| AudioFrame::from_bytes(c.try_into().expect("frame size")))
                .collect();
            self.pending.drain(..whole);
            // 100 Hz frames, 3 frames per bar: the strip covers ~0.85 s.
            for f in frames {
                self.bar_peak = self.bar_peak.max(f.peak_dbfs);
                self.frames_in_bar += 1;
                if self.frames_in_bar == 3 {
                    self.peaks.remove(0);
                    self.peaks.push(self.bar_peak);
                    self.frames_in_bar = 0;
                    self.bar_peak = -120.0;
                }
            }
        }

        fn strip(&self) -> String {
            self.peaks
                .iter()
                .map(|db| {
                    let t = ((db + 55.0) / 50.0).clamp(0.0, 1.0);
                    BAR_GLYPHS[(t * 7.0).round() as usize]
                })
                .collect()
        }
    }

    struct Overlay {
        panel: Retained<NSPanel>,
        effect: Retained<NSVisualEffectView>,
        label: Retained<NSTextField>,
        mtm: MainThreadMarker,
        shown_text: String,
        alpha: f64,
    }

    impl Overlay {
        fn new(mtm: MainThreadMarker) -> Self {
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(MIN_WIDTH, 60.0));
            unsafe {
                let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                    NSPanel::alloc(mtm),
                    rect,
                    NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                    NSBackingStoreType::Buffered,
                    false,
                );
                panel.setReleasedWhenClosed(false);
                panel.setFloatingPanel(true);
                panel.setHidesOnDeactivate(false);
                panel.setLevel(25); // NSStatusWindowLevel
                panel.setOpaque(false);
                panel.setBackgroundColor(Some(&NSColor::clearColor()));
                panel.setIgnoresMouseEvents(true);
                panel.setHasShadow(true);
                panel.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                if let Some(dark) = NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) {
                    panel.setAppearance(Some(&dark));
                }

                // Smoke glass: dark HUD vibrancy blurring whatever is behind.
                let effect =
                    NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
                effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
                effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                effect.setState(NSVisualEffectState::Active);
                effect.setWantsLayer(true);
                let layer: Option<Retained<AnyObject>> = msg_send![&*effect, layer];
                if let Some(layer) = layer {
                    let _: () = msg_send![&*layer, setCornerRadius: CORNER];
                    let _: () = msg_send![&*layer, setMasksToBounds: true];
                }

                let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
                label.setTextColor(Some(&NSColor::whiteColor()));
                label.setFont(Some(&NSFont::systemFontOfSize_weight(20.0, 0.23)));
                label.setAlignment(NSTextAlignment::Center);
                label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
                label.setMaximumNumberOfLines(5);
                label.setPreferredMaxLayoutWidth(MAX_TEXT_WIDTH);
                label.setDrawsBackground(false);

                effect.addSubview(&label);
                panel.setContentView(Some(&effect));
                panel.setAlphaValue(0.0);

                Self {
                    panel,
                    effect,
                    label,
                    mtm,
                    shown_text: String::new(),
                    alpha: 0.0,
                }
            }
        }

        fn set_text(&mut self, text: &str) {
            if text == self.shown_text {
                return;
            }
            self.shown_text = text.to_string();
            {
                self.label.setStringValue(&NSString::from_str(text));
                let fit = self.label.fittingSize();
                let text_w = fit.width.min(MAX_TEXT_WIDTH);
                let w = (text_w + PAD_X * 2.0).max(MIN_WIDTH);
                let h = fit.height + PAD_Y * 2.0;
                let screen =
                    NSScreen::mainScreen(self.mtm)
                        .map(|s| s.frame())
                        .unwrap_or(NSRect::new(
                            NSPoint::new(0.0, 0.0),
                            NSSize::new(1440.0, 900.0),
                        ));
                let x = screen.origin.x + (screen.size.width - w) / 2.0;
                let y = screen.origin.y + (screen.size.height - h) / 2.0;
                self.panel
                    .setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)), true);
                self.effect
                    .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
                self.label.setFrame(NSRect::new(
                    NSPoint::new((w - text_w) / 2.0, PAD_Y),
                    NSSize::new(text_w, fit.height),
                ));
            }
        }

        /// Ease alpha toward visible/hidden; order the panel in or out.
        fn animate(&mut self, visible: bool) {
            let target = if visible { 1.0 } else { 0.0 };
            if (self.alpha - target).abs() < 1e-3 {
                return;
            }
            let was_hidden = self.alpha <= 0.0;
            let step = if visible { 0.25 } else { 0.12 };
            self.alpha = if visible {
                (self.alpha + step).min(1.0)
            } else {
                (self.alpha - step).max(0.0)
            };
            {
                self.panel.setAlphaValue(self.alpha);
                if visible && was_hidden {
                    self.panel.orderFrontRegardless();
                }
                if self.alpha <= 0.0 {
                    self.panel.orderOut(None);
                    self.shown_text.clear();
                }
            }
        }
    }

    /// Run the AppKit event loop for `secs` seconds.
    fn pump(app: &NSApplication, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end {
            let deadline = NSDate::dateWithTimeIntervalSinceNow(TICK);
            let ev = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&deadline),
                    NSDefaultRunLoopMode,
                    true,
                )
            };
            if let Some(e) = ev {
                app.sendEvent(&e);
            }
        }
    }

    impl Overlay {
        /// Render the panel's content view to a PNG (demo/testing aid).
        fn snapshot(&self, path: &str) {
            let bounds = self.effect.bounds();
            let Some(rep) = self.effect.bitmapImageRepForCachingDisplayInRect(bounds) else {
                eprintln!("snapshot: no bitmap rep");
                return;
            };
            self.effect
                .cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
            let props = NSDictionary::new();
            let data = unsafe {
                rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)
            };
            match data {
                Some(d) if d.writeToFile_atomically(&NSString::from_str(path), true) => {
                    println!("snapshot written: {path}")
                }
                _ => eprintln!("snapshot: failed to write {path}"),
            }
        }
    }

    /// A message that stays up for a while after the session ends.
    struct Linger {
        text: String,
        until: Instant,
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("voxtype-osd-macos must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();

        let dir = Config::runtime_dir();
        let state_path = dir.join("state");
        let session_path = dir.join("recording.json");
        let outcome_path = dir.join("outcome.json");
        let text_path = dir.join("last_text");

        let mut overlay = Overlay::new(mtm);

        // `--demo "<text>" [--snapshot <file.png>]`: show the overlay with
        // fixed text for a few seconds (or render it to a PNG and exit), so
        // the look can be checked without dictating.
        let args: Vec<String> = std::env::args().collect();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1).cloned())
        };
        if let Some(text) = flag("--demo") {
            overlay.set_text(&text.replace("\\n", "\n"));
            while overlay.alpha < 1.0 {
                overlay.animate(true);
            }
            pump(&app, 0.5);
            match flag("--snapshot") {
                Some(path) => overlay.snapshot(&path),
                None => pump(&app, 4.0),
            }
            return;
        }

        let mut levels = Levels::new(dir.join("audio.sock"));
        let started_ms = now_ms();
        let mut last_outcome_ms = read_json(&outcome_path)
            .and_then(|v| v["at_ms"].as_u64())
            .unwrap_or(0)
            .max(started_ms);
        let mut linger: Option<Linger> = None;
        let mut local_rec_start: Option<Instant> = None;
        let parent = std::os::unix::process::parent_id();

        loop {
            // Pump AppKit events: wait up to one tick, then drain the rest.
            let until = NSDate::dateWithTimeIntervalSinceNow(TICK);
            let mut wait = Some(until);
            loop {
                let deadline = wait.take().unwrap_or_else(NSDate::distantPast);
                let ev = unsafe {
                    app.nextEventMatchingMask_untilDate_inMode_dequeue(
                        NSEventMask::Any,
                        Some(&deadline),
                        NSDefaultRunLoopMode,
                        true,
                    )
                };
                match ev {
                    Some(e) => app.sendEvent(&e),
                    None => break,
                }
            }

            // The daemon owns us; exit if it is gone.
            if std::os::unix::process::parent_id() != parent {
                return;
            }

            let state = read_trimmed(&state_path).unwrap_or_default();

            // A fresh outcome replaces whatever is lingering.
            if let Some(o) = read_json(&outcome_path) {
                let at = o["at_ms"].as_u64().unwrap_or(0);
                if at > last_outcome_ms {
                    last_outcome_ms = at;
                    let suppressed = o["suppressed"].as_bool().unwrap_or(false);
                    let msg = match o["outcome"].as_str().unwrap_or("") {
                        "ok" => read_trimmed(&text_path).map(|t| {
                            let n = t.chars().count();
                            let shown: String = if n > MAX_TEXT_CHARS {
                                let mut s: String = t.chars().take(MAX_TEXT_CHARS).collect();
                                s.push('…');
                                s
                            } else {
                                t
                            };
                            let secs = (1.8 + n as f64 * 0.03).clamp(2.0, 6.0);
                            (shown, secs)
                        }),
                        "no_speech" | "empty" => Some(("No speech detected".to_string(), 1.4)),
                        "too_short" => None,
                        "error" => Some(("Transcription failed".to_string(), 2.5)),
                        "output_failed" => Some(("Couldn't type the text".to_string(), 2.5)),
                        _ => None,
                    };
                    linger = match msg {
                        Some((text, secs)) if !suppressed => Some(Linger {
                            text,
                            until: Instant::now() + Duration::from_secs_f64(secs),
                        }),
                        _ => None,
                    };
                }
            }

            let content: Option<String> = match state.as_str() {
                "recording" | "streaming" => {
                    linger = None;
                    let start_ms =
                        read_json(&session_path).and_then(|v| v["started_at_ms"].as_u64());
                    let local = *local_rec_start.get_or_insert_with(|| {
                        levels.reset();
                        Instant::now()
                    });
                    let elapsed = match start_ms {
                        Some(ms) => now_ms().saturating_sub(ms) / 1000,
                        None => local.elapsed().as_secs(),
                    };
                    levels.poll();
                    Some(format!(
                        "{}\n●  Listening   {}:{:02}",
                        levels.strip(),
                        elapsed / 60,
                        elapsed % 60
                    ))
                }
                "transcribing" => {
                    local_rec_start = None;
                    Some("Transcribing…".to_string())
                }
                _ => {
                    local_rec_start = None;
                    match &linger {
                        Some(l) if Instant::now() < l.until => Some(l.text.clone()),
                        _ => {
                            linger = None;
                            None
                        }
                    }
                }
            };

            match content {
                Some(text) => {
                    overlay.set_text(&text);
                    overlay.animate(true);
                }
                None => overlay.animate(false),
            }
        }
    }
}
