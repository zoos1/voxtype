//! `voxtype-osd-macos` — native macOS on-screen display for voxtype.
//!
//! A small borderless, click-through, non-activating panel centered on the
//! main screen: a see-through smoke tint with rounded corners, no border and
//! no window shadow, white text in JetBrains Mono (falls back to the system
//! monospaced font). It shows:
//!
//! - while recording: a live visualizer and the elapsed time
//! - while transcribing: "Transcribing…"
//! - when text is delivered: the transcript, briefly, then fades out
//! - when nothing was heard: "No speech detected"
//!
//! Two visualizers, chosen by `[osd] macos_visualizer` in config.toml (or
//! the `VOXTYPE_OSD_VIZ` environment variable), re-read at each recording:
//!
//! - `dots` (default): a scrolling waveform of white dots, fed by the
//!   daemon's `audio.sock` level stream
//! - `spectrum`: a scrolling spectrogram, purple → pink → yellow by loudness,
//!   computed here from the default input device while recording
//!
//! Apart from the spectrogram's own microphone tap, it reads only what the
//! daemon publishes in its runtime dir (`state`, `recording.json`,
//! `outcome.json`, `last_text`, `audio.sock`), so it can crash or restart
//! without affecting dictation. The daemon's OSD supervisor launches it
//! through the `voxtype-osd` launcher.
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
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use objc2::rc::Retained;
    use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSApplication,
        NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageFileType,
        NSBitmapImageRep, NSBox, NSBoxType, NSColor, NSDeviceRGBColorSpace, NSEventMask, NSFont,
        NSImage, NSImageScaling, NSImageView, NSLineBreakMode, NSPanel, NSScreen,
        NSTextAlignment, NSTextField, NSTitlePosition, NSView, NSWindowCollectionBehavior,
        NSWindowStyleMask,
    };
    use objc2_foundation::{
        NSDate, NSDefaultRunLoopMode, NSDictionary, NSPoint, NSRect, NSSize, NSString,
    };

    use voxtype::audio::levels::{AudioFrame, FRAME_BYTES};
    use voxtype::config::Config;

    const TICK: f64 = 1.0 / 30.0;

    // Layout, in points. About 40% of the first version's size.
    const FONT_SIZE: f64 = 12.0;
    const FONT_NAMES: [&str; 2] = ["JetBrainsMono-Light", "JetBrainsMonoNL-Light"];
    // Transcript lines wrap at ~40 characters: phone feeds in portrait
    // land at 30-50 (Baymard: 50-75 is for wider desktop columns).
    const LINE_CHARS: usize = 40;
    const MAX_TEXT_WIDTH: f64 = 290.0; // fallback if the font cannot be measured
    const PAD_X: f64 = 12.0;
    const PAD_Y: f64 = 8.0;
    const GAP: f64 = 10.0;
    const MIN_WIDTH: f64 = 72.0;
    const CORNER: f64 = 10.0;
    const MAX_TEXT_CHARS: usize = 400;
    const MAX_LINES: isize = 10;
    // Transcript display time: ~238 wpm adult silent reading (Brysbaert
    // 2019), so 0.25 s a word plus a second to find the panel.
    const SECS_PER_WORD: f64 = 0.25;
    const LINGER_BASE: f64 = 1.0;
    const LINGER_MIN: f64 = 1.5;
    const LINGER_MAX: f64 = 12.0;
    /// Smoke: near-black grey, mostly see-through.
    const SMOKE: (f64, f64, f64, f64) = (0.10, 0.10, 0.11, 0.55);

    // Visualizer canvas: points, and pixels at 2x for Retina.
    const VIZ_W: f64 = 150.0;
    const VIZ_H: f64 = 22.0;
    const SCALE: usize = 2;
    const PX_W: usize = VIZ_W as usize * SCALE;
    const PX_H: usize = VIZ_H as usize * SCALE;

    const DOT_COLS: usize = 30;
    const DOT_ROWS_HALF: usize = 3; // dots above and below the center line
    const SPEC_COLS: usize = 64;
    const SPEC_ROWS: usize = 24;
    const FFT_N: usize = 2048;
    const BAND_LO: f32 = 80.0;
    const BAND_HI: f32 = 8000.0;

    // Voice gate: the panel shows from speech onset until a pause. Levels
    // are dB above a per-band noise floor that adapts to the room.
    const VOICE_ON_DB: f32 = 14.0;
    const VOICE_OFF_DB: f32 = 8.0;
    const VOICE_ON_TICKS: u32 = 4; // ~130 ms: a syllable, not a key click
    const HANG_TICKS: u32 = 36; // ~1.2 s of quiet before the panel fades
    const FLOOR_WINDOW: usize = 90; // ticks, ~3 s
    const DIGITAL_SILENCE_DB: f32 = -120.0;
    const WARMUP_DB: f32 = -100.0; // band mean below this is mic warm-up
    const WARMUP_TICKS: u32 = 6; // ~200 ms
    const PEAK_FALL_DB: f32 = 0.2; // per tick, ~6 dB/s
    const MIN_RANGE_DB: f32 = 30.0; // excess that reads as full yellow, at least
    const SHOW_DB: f32 = 6.0; // excess below this draws nothing

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

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Viz {
        Dots,
        Spectrum,
    }

    impl Viz {
        fn parse(s: &str) -> Viz {
            match s.trim().to_ascii_lowercase().as_str() {
                "spectrum" | "spectrogram" => Viz::Spectrum,
                _ => Viz::Dots,
            }
        }

        /// `VOXTYPE_OSD_VIZ`, else `[osd] macos_visualizer`, else dots.
        fn configured() -> Viz {
            if let Ok(v) = std::env::var("VOXTYPE_OSD_VIZ") {
                return Viz::parse(&v);
            }
            dirs::config_dir()
                .map(|d| d.join("voxtype").join("config.toml"))
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|s| s.parse::<toml::Value>().ok())
                .and_then(|v| {
                    v.get("osd")?
                        .get("macos_visualizer")?
                        .as_str()
                        .map(Viz::parse)
                })
                .unwrap_or(Viz::Dots)
        }
    }

    // ---------------------------------------------------------------- pixels

    /// RGBA, premultiplied alpha, row 0 at the top.
    struct Canvas {
        px: Vec<u8>,
    }

    impl Canvas {
        fn new() -> Self {
            Self {
                px: vec![0; PX_W * PX_H * 4],
            }
        }

        fn clear(&mut self) {
            self.px.iter_mut().for_each(|b| *b = 0);
        }

        /// Source-over blend of a straight-alpha color with coverage `a`.
        fn blend(&mut self, x: i32, y: i32, rgb: (f32, f32, f32), a: f32) {
            if x < 0 || y < 0 || x as usize >= PX_W || y as usize >= PX_H || a <= 0.0 {
                return;
            }
            let a = a.min(1.0);
            let i = (y as usize * PX_W + x as usize) * 4;
            let src = [rgb.0 * a, rgb.1 * a, rgb.2 * a, a];
            for (k, s) in src.iter().enumerate() {
                let d = self.px[i + k] as f32 / 255.0;
                self.px[i + k] = ((s + d * (1.0 - a)) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }

        /// Anti-aliased filled circle, in pixel units.
        fn disc(&mut self, cx: f32, cy: f32, r: f32, rgb: (f32, f32, f32), alpha: f32) {
            let (x0, x1) = ((cx - r - 1.0).floor() as i32, (cx + r + 1.0).ceil() as i32);
            let (y0, y1) = ((cy - r - 1.0).floor() as i32, (cy + r + 1.0).ceil() as i32);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                    let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                    self.blend(x, y, rgb, cov * alpha);
                }
            }
        }

    }

    /// Waveform of white dots: one column per time step, mirrored around the
    /// center line, newest on the right. `amps` are 0..1.
    fn draw_dots(c: &mut Canvas, amps: &[f32]) {
        c.clear();
        let step_x = PX_W as f32 / DOT_COLS as f32;
        let step_y = PX_H as f32 / (DOT_ROWS_HALF as f32 * 2.0 + 1.0);
        let mid = PX_H as f32 / 2.0;
        let r = 1.15 * SCALE as f32;
        for (i, &t) in amps.iter().enumerate() {
            let cx = step_x * (i as f32 + 0.5);
            let lit = (t * DOT_ROWS_HALF as f32).round() as usize;
            for k in 0..=DOT_ROWS_HALF {
                let alpha = if k == 0 {
                    0.45 + 0.55 * t.min(1.0)
                } else if k <= lit {
                    0.95
                } else {
                    0.0
                };
                c.disc(cx, mid - k as f32 * step_y, r, (1.0, 1.0, 1.0), alpha);
                if k > 0 {
                    c.disc(cx, mid + k as f32 * step_y, r, (1.0, 1.0, 1.0), alpha);
                }
            }
        }
    }

    /// Purple → pink → yellow, by loudness `t` in 0..1.
    fn heat(t: f32) -> (f32, f32, f32) {
        const STOPS: [(f32, (f32, f32, f32)); 4] = [
            (0.0, (0.29, 0.08, 0.50)),  // deep purple
            (0.40, (0.55, 0.17, 0.80)), // purple
            (0.70, (1.00, 0.30, 0.62)), // pink
            (1.0, (1.00, 0.88, 0.40)),  // yellow
        ];
        let t = t.clamp(0.0, 1.0);
        for w in STOPS.windows(2) {
            let ((t0, c0), (t1, c1)) = (w[0], w[1]);
            if t <= t1 {
                let f = (t - t0) / (t1 - t0);
                return (
                    c0.0 + (c1.0 - c0.0) * f,
                    c0.1 + (c1.1 - c0.1) * f,
                    c0.2 + (c1.2 - c0.2) * f,
                );
            }
        }
        STOPS[3].1
    }

    /// Spectrogram: time left → right, frequency bottom → top. Cells are
    /// interpolated per pixel, so the image is continuous (no grid seams).
    /// `t` = 0 is silence and draws nothing; low `t` is dark purple, the
    /// loudest parts yellow. The oldest columns fade out on the left.
    fn draw_spectrum(c: &mut Canvas, cols: &VecDeque<[f32; SPEC_ROWS]>) {
        c.clear();
        let n = cols.len().min(SPEC_COLS);
        if n == 0 {
            return;
        }
        let skip = cols.len() - n;
        let offset = (SPEC_COLS - n) as f32;
        let cw = PX_W as f32 / SPEC_COLS as f32;
        let rh = PX_H as f32 / SPEC_ROWS as f32;
        let at = |i: f32, r: f32| -> f32 {
            let i = (i.max(0.0) as usize).min(n - 1);
            let r = (r.max(0.0) as usize).min(SPEC_ROWS - 1);
            cols[skip + i][r]
        };
        for y in 0..PX_H {
            let v = ((PX_H - 1 - y) as f32 + 0.5) / rh - 0.5;
            let (r0, fv) = (v.floor(), v - v.floor());
            for x in 0..PX_W {
                let u = (x as f32 + 0.5) / cw - 0.5 - offset;
                if u < -0.5 {
                    continue; // left of the oldest column
                }
                let (i0, fu) = (u.floor(), u - u.floor());
                let top = at(i0, r0 + 1.0) * (1.0 - fu) + at(i0 + 1.0, r0 + 1.0) * fu;
                let bot = at(i0, r0) * (1.0 - fu) + at(i0 + 1.0, r0) * fu;
                let t = bot * (1.0 - fv) + top * fv;
                if t <= 0.002 {
                    continue;
                }
                let fade = (x as f32 / (PX_W as f32 * 0.12)).min(1.0);
                let a = (t / 0.06).min(1.0) * 0.95 * fade;
                c.blend(x as i32, y as i32, heat(t), a);
            }
        }
    }

    fn canvas_image(c: &Canvas) -> Option<Retained<NSImage>> {
        unsafe {
            let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                PX_W as isize,
                PX_H as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                (PX_W * 4) as isize,
                32,
            )?;
            let dst = rep.bitmapData();
            if dst.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(c.px.as_ptr(), dst, c.px.len());
            rep.setSize(NSSize::new(VIZ_W, VIZ_H));
            let img = NSImage::initWithSize(NSImage::alloc(), NSSize::new(VIZ_W, VIZ_H));
            img.addRepresentation(&rep);
            Some(img)
        }
    }

    // ------------------------------------------------------------ audio data

    /// Input-level history fed from the daemon's audio socket.
    struct Levels {
        sock: PathBuf,
        stream: Option<UnixStream>,
        pending: Vec<u8>,
        amps: Vec<f32>,
        frames_in_col: usize,
        col_peak: f32,
        floor: Floor,
        gate: VoiceGate,
        shown: bool,
    }

    impl Levels {
        fn new(sock: PathBuf) -> Self {
            Self {
                sock,
                stream: None,
                pending: Vec::new(),
                amps: vec![0.0; DOT_COLS],
                frames_in_col: 0,
                col_peak: -120.0,
                floor: Floor::new(),
                gate: VoiceGate::default(),
                shown: false,
            }
        }

        fn reset(&mut self) {
            self.amps.iter_mut().for_each(|a| *a = 0.0);
            self.frames_in_col = 0;
            self.col_peak = -120.0;
            self.floor = Floor::new();
            self.gate = VoiceGate::default();
            self.shown = false;
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
            if let Some(tick_peak) = frames.iter().map(|f| f.peak_dbfs).reduce(f32::max) {
                if tick_peak > DIGITAL_SILENCE_DB {
                    let floor = self.floor.update(tick_peak);
                    self.shown = self.gate.update(tick_peak - floor);
                }
            }
            // 100 Hz frames, 4 per column: the waveform covers ~1.2 s.
            for f in frames {
                self.col_peak = self.col_peak.max(f.peak_dbfs);
                self.frames_in_col += 1;
                if self.frames_in_col == 4 {
                    let t = ((self.col_peak + 55.0) / 45.0).clamp(0.0, 1.0);
                    self.amps.remove(0);
                    self.amps.push(t);
                    self.frames_in_col = 0;
                    self.col_peak = -120.0;
                }
            }
        }
    }

    fn mic_error(e: cpal::StreamError) {
        eprintln!("voxtype-osd-macos: microphone stream error: {e}");
    }

    /// Microphone tap for the spectrogram, open only while recording.
    struct Mic {
        _stream: cpal::Stream,
        ring: Arc<Mutex<VecDeque<f32>>>,
        rate: f32,
    }

    impl Mic {
        fn open() -> Option<Mic> {
            let dev = cpal::default_host().default_input_device()?;
            let cfg = dev.default_input_config().ok()?;
            let channels = cfg.channels().max(1) as usize;
            let rate = cfg.sample_rate().0 as f32;
            let ring = Arc::new(Mutex::new(VecDeque::with_capacity(FFT_N * 4)));
            let push = {
                let ring = ring.clone();
                move |mono: &mut dyn Iterator<Item = f32>| {
                    if let Ok(mut r) = ring.lock() {
                        r.extend(mono);
                        let excess = r.len().saturating_sub(FFT_N * 4);
                        r.drain(..excess);
                    }
                }
            };
            let stream_cfg: cpal::StreamConfig = cfg.clone().into();
            let stream = match cfg.sample_format() {
                cpal::SampleFormat::F32 => dev
                    .build_input_stream(
                        &stream_cfg,
                        move |d: &[f32], _: &cpal::InputCallbackInfo| {
                            push(&mut d.chunks(channels).map(|f| f[0]))
                        },
                        mic_error,
                        None,
                    )
                    .ok()?,
                cpal::SampleFormat::I16 => dev
                    .build_input_stream(
                        &stream_cfg,
                        move |d: &[i16], _: &cpal::InputCallbackInfo| {
                            push(&mut d.chunks(channels).map(|f| f[0] as f32 / 32768.0))
                        },
                        mic_error,
                        None,
                    )
                    .ok()?,
                _ => return None,
            };
            stream.play().ok()?;
            Some(Mic {
                _stream: stream,
                ring,
                rate,
            })
        }

        fn latest(&self) -> Option<Vec<f32>> {
            let r = self.ring.lock().ok()?;
            if r.len() < FFT_N {
                return None;
            }
            Some(r.iter().skip(r.len() - FFT_N).copied().collect())
        }
    }

    /// In-place iterative radix-2 FFT; `re.len()` must be a power of two.
    fn fft(re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
        let mut j = 0;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let ang = -2.0 * std::f32::consts::PI / len as f32;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (s, c) = (ang * k as f32).sin_cos();
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }

    /// Log-spaced band levels in dBFS (a full-scale sine reads 0 dB), from
    /// BAND_LO to BAND_HI.
    fn bands(samples: &[f32], rate: f32) -> [f32; SPEC_ROWS] {
        let n = samples.len();
        let mut re: Vec<f32> = samples
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
                s * w
            })
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let norm = n as f32 / 4.0; // Hann window gain 0.5, one-sided x2
        let (lo, hi) = (BAND_LO, BAND_HI);
        let mut out = [0.0; SPEC_ROWS];
        for (b, slot) in out.iter_mut().enumerate() {
            let f0 = lo * (hi / lo).powf(b as f32 / SPEC_ROWS as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / SPEC_ROWS as f32);
            let k0 = ((f0 * n as f32 / rate) as usize).max(1);
            let k1 = ((f1 * n as f32 / rate).ceil() as usize).clamp(k0 + 1, n / 2);
            let peak = (k0..k1)
                .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt() / norm)
                .fold(0.0f32, f32::max);
            *slot = 20.0 * peak.max(1e-9).log10();
        }
        out
    }

    /// Room noise floor: the 10th percentile of the last FLOOR_WINDOW levels.
    /// Speech has gaps between syllables, so this sits on the room's level
    /// even while you talk, and one odd quiet frame cannot drag it down.
    struct Floor {
        hist: VecDeque<f32>,
    }

    impl Floor {
        fn new() -> Self {
            Self {
                hist: VecDeque::with_capacity(FLOOR_WINDOW + 1),
            }
        }

        fn update(&mut self, level: f32) -> f32 {
            if self.hist.len() == FLOOR_WINDOW {
                self.hist.pop_front();
            }
            self.hist.push_back(level);
            let mut v: Vec<f32> = self.hist.iter().copied().collect();
            v.sort_by(f32::total_cmp);
            v[(v.len() - 1) / 10]
        }
    }

    /// Speech detector with hysteresis and a hangover, fed once per tick
    /// with the level above the noise floor (dB).
    #[derive(Default)]
    struct VoiceGate {
        over: u32,
        voiced: bool,
        hang: u32,
    }

    impl VoiceGate {
        /// Returns whether the panel should show: from speech onset until
        /// HANG_TICKS of quiet.
        fn update(&mut self, excess: f32) -> bool {
            self.over = if excess > VOICE_ON_DB { self.over + 1 } else { 0 };
            self.voiced = self.over >= VOICE_ON_TICKS || (self.voiced && excess > VOICE_OFF_DB);
            self.hang = if self.voiced {
                HANG_TICKS
            } else {
                self.hang.saturating_sub(1)
            };
            self.hang > 0
        }
    }

    /// Spectrogram state: smoothed band levels, per-band noise floors, and a
    /// decaying loudness peak so the loudest recent speech reads yellow and
    /// quieter speech reads purple, whatever the mic gain.
    struct Spectro {
        cols: VecDeque<[f32; SPEC_ROWS]>,
        smooth: [f32; SPEC_ROWS],
        floor: [f32; SPEC_ROWS],
        floors: Vec<Floor>,
        frames: u32,
        peak: f32,
        started: bool,
        gate: VoiceGate,
        shown: bool,
    }

    impl Spectro {
        fn new() -> Self {
            Self {
                cols: VecDeque::with_capacity(SPEC_COLS + 1),
                smooth: [0.0; SPEC_ROWS],
                floor: [0.0; SPEC_ROWS],
                floors: (0..SPEC_ROWS).map(|_| Floor::new()).collect(),
                frames: 0,
                peak: MIN_RANGE_DB,
                started: false,
                gate: VoiceGate::default(),
                shown: false,
            }
        }

        /// Add one analysis frame (band levels in dB); returns whether the
        /// panel should show. Columns are blank while no one is speaking.
        fn push(&mut self, db: [f32; SPEC_ROWS]) -> bool {
            // The input warms up with zeros or near-zeros for a few buffers;
            // they say nothing about the room, so skip them. A real mic in a
            // quiet room still averages well above WARMUP_DB per band.
            self.frames += 1;
            let mean = db.iter().sum::<f32>() / SPEC_ROWS as f32;
            if self.frames <= WARMUP_TICKS || mean < WARMUP_DB {
                return self.shown;
            }
            if !self.started {
                self.started = true;
                self.smooth = db;
            }
            let mut excess = [0.0f32; SPEC_ROWS];
            for b in 0..SPEC_ROWS {
                let k = if db[b] > self.smooth[b] { 0.65 } else { 0.35 };
                self.smooth[b] += (db[b] - self.smooth[b]) * k;
                self.floor[b] = self.floors[b].update(self.smooth[b]);
                excess[b] = (self.smooth[b] - self.floor[b]).max(0.0);
            }
            // Voice energy sits in a handful of bands: average the top 8.
            let mut sorted = excess;
            sorted.sort_by(|a, b| b.total_cmp(a));
            let voice = sorted[..8].iter().sum::<f32>() / 8.0;
            self.shown = self.gate.update(voice);

            let top = excess.iter().copied().fold(0.0f32, f32::max);
            self.peak = (self.peak - PEAK_FALL_DB).max(top).max(MIN_RANGE_DB);
            let mut col = [0.0f32; SPEC_ROWS];
            if self.gate.voiced {
                for (c, &e) in col.iter_mut().zip(&excess) {
                    if e >= SHOW_DB {
                        let t = ((e - SHOW_DB) / (self.peak - SHOW_DB)).clamp(0.0, 1.0);
                        *c = t.powf(1.2).max(0.02);
                    }
                }
            }
            if self.cols.len() == SPEC_COLS {
                self.cols.pop_front();
            }
            self.cols.push_back(col);
            self.shown
        }
    }

    // ---------------------------------------------------------------- window

    struct Overlay {
        panel: Retained<NSPanel>,
        root: Retained<NSView>,
        smoke: Retained<NSBox>,
        label: Retained<NSTextField>,
        viz: Retained<NSImageView>,
        canvas: Canvas,
        mtm: MainThreadMarker,
        shown_key: String,
        alpha: f64,
        line_w: f64,
    }

    fn overlay_font() -> Retained<NSFont> {
        FONT_NAMES
            .iter()
            .find_map(|n| NSFont::fontWithName_size(&NSString::from_str(n), FONT_SIZE))
            .unwrap_or_else(|| NSFont::monospacedSystemFontOfSize_weight(FONT_SIZE, -0.4))
    }

    impl Overlay {
        fn new(mtm: MainThreadMarker) -> Self {
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(MIN_WIDTH, 32.0));
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
                // The window shadow traced a hairline around the panel.
                panel.setHasShadow(false);
                panel.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
                if let Some(dark) = NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua) {
                    panel.setAppearance(Some(&dark));
                }

                let root = NSView::initWithFrame(NSView::alloc(mtm), rect);

                // Plain see-through tint: no blur material, no border.
                let smoke = NSBox::initWithFrame(NSBox::alloc(mtm), rect);
                smoke.setBoxType(NSBoxType::Custom);
                smoke.setTitlePosition(NSTitlePosition::NoTitle);
                smoke.setBorderWidth(0.0);
                smoke.setBorderColor(&NSColor::clearColor());
                smoke.setCornerRadius(CORNER);
                smoke.setContentViewMargins(NSSize::new(0.0, 0.0));
                smoke.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
                    SMOKE.0, SMOKE.1, SMOKE.2, SMOKE.3,
                ));
                root.addSubview(&smoke);

                let viz = NSImageView::initWithFrame(
                    NSImageView::alloc(mtm),
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(VIZ_W, VIZ_H)),
                );
                viz.setImageScaling(NSImageScaling::ScaleAxesIndependently);
                viz.setHidden(true);
                root.addSubview(&viz);

                let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
                label.setTextColor(Some(&NSColor::whiteColor()));
                label.setFont(Some(&overlay_font()));
                label.setAlignment(NSTextAlignment::Left);
                label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
                label.setMaximumNumberOfLines(MAX_LINES);
                label.setDrawsBackground(false);
                // Line width = LINE_CHARS of this font, measured once.
                label.setPreferredMaxLayoutWidth(10_000.0);
                label.setStringValue(&NSString::from_str(&"0".repeat(LINE_CHARS)));
                let measured = label.fittingSize().width.ceil();
                let line_w = if measured > 50.0 { measured } else { MAX_TEXT_WIDTH };
                label.setPreferredMaxLayoutWidth(line_w);
                label.setStringValue(&NSString::from_str(""));
                root.addSubview(&label);

                panel.setContentView(Some(&root));
                panel.setAlphaValue(0.0);

                Self {
                    panel,
                    root,
                    smoke,
                    label,
                    viz,
                    canvas: Canvas::new(),
                    mtm,
                    shown_key: String::new(),
                    alpha: 0.0,
                    line_w,
                }
            }
        }

        fn place(&self, w: f64, h: f64) {
            let screen = NSScreen::mainScreen(self.mtm)
                .map(|s| s.frame())
                .unwrap_or(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(1440.0, 900.0),
                ));
            let x = (screen.origin.x + (screen.size.width - w) / 2.0).round();
            let y = (screen.origin.y + (screen.size.height - h) / 2.0).round();
            let local = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
            self.panel
                .setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)), true);
            self.root.setFrame(local);
            self.smoke.setFrame(local);
        }

        /// Text only: a status line or the transcript.
        fn show_text(&mut self, text: &str) {
            let key = format!("t:{text}");
            if key == self.shown_key {
                return;
            }
            self.shown_key = key;
            self.viz.setHidden(true);
            self.label.setAlignment(NSTextAlignment::Left);
            self.label.setStringValue(&NSString::from_str(text));
            let fit = self.label.fittingSize();
            let text_w = fit.width.min(self.line_w).ceil();
            let w = (text_w + PAD_X * 2.0).max(MIN_WIDTH);
            let h = (fit.height + PAD_Y * 2.0).ceil();
            self.place(w, h);
            self.label.setFrame(NSRect::new(
                NSPoint::new(((w - text_w) / 2.0).round(), PAD_Y),
                NSSize::new(text_w, fit.height.ceil()),
            ));
        }

        /// Visualizer plus the elapsed time to its right.
        fn show_recording(&mut self, timer: &str) {
            if let Some(img) = canvas_image(&self.canvas) {
                self.viz.setImage(Some(&img));
            }
            let key = format!("r:{timer}");
            if key == self.shown_key {
                return;
            }
            self.shown_key = key;
            self.label.setAlignment(NSTextAlignment::Left);
            self.label.setStringValue(&NSString::from_str(timer));
            let fit = self.label.fittingSize();
            let w = (PAD_X + VIZ_W + GAP + fit.width + PAD_X).ceil();
            let h = VIZ_H + PAD_Y * 2.0;
            self.place(w, h);
            self.viz.setFrame(NSRect::new(
                NSPoint::new(PAD_X, PAD_Y),
                NSSize::new(VIZ_W, VIZ_H),
            ));
            self.viz.setHidden(false);
            self.label.setFrame(NSRect::new(
                NSPoint::new(PAD_X + VIZ_W + GAP, ((h - fit.height) / 2.0).round()),
                NSSize::new(fit.width.ceil(), fit.height.ceil()),
            ));
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
            self.panel.setAlphaValue(self.alpha);
            if visible && was_hidden {
                self.panel.orderFrontRegardless();
            }
            if self.alpha <= 0.0 {
                self.panel.orderOut(None);
                self.shown_key.clear();
            }
        }

        /// Render the panel to a PNG (demo/testing aid). With `checker`, the
        /// panel is composited over a light/dark checkerboard so its
        /// transparency shows; otherwise the PNG keeps its alpha channel.
        fn snapshot(&self, path: &str, checker: bool) {
            let bounds = self.root.bounds();
            let (w, h) = (
                (bounds.size.width * 2.0) as usize,
                (bounds.size.height * 2.0) as usize,
            );
            let rep = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(),
                    std::ptr::null_mut(),
                    w as isize,
                    h as isize,
                    8,
                    4,
                    true,
                    false,
                    NSDeviceRGBColorSpace,
                    (w * 4) as isize,
                    32,
                )
            };
            let Some(rep) = rep else {
                eprintln!("snapshot: no bitmap rep");
                return;
            };
            rep.setSize(bounds.size);
            self.root.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
            if checker {
                let data = rep.bitmapData();
                let px = unsafe { std::slice::from_raw_parts_mut(data, w * h * 4) };
                for y in 0..h {
                    for x in 0..w {
                        let i = (y * w + x) * 4;
                        let light = ((x / 24) + (y / 24)) % 2 == 0;
                        let bg: [f32; 3] = if light {
                            [0.93, 0.80, 0.30]
                        } else {
                            [0.20, 0.45, 0.85]
                        };
                        let a = px[i + 3] as f32 / 255.0;
                        for k in 0..3 {
                            let v = px[i + k] as f32 / 255.0 + bg[k] * (1.0 - a);
                            px[i + k] = (v * 255.0).round().clamp(0.0, 255.0) as u8;
                        }
                        px[i + 3] = 255;
                    }
                }
            }
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

    /// Run the AppKit event loop for `secs` seconds, calling `each` per tick.
    fn pump(app: &NSApplication, secs: f64, mut each: impl FnMut()) {
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
            each();
        }
    }

    /// Speech-like test signal for `--demo-rec`: syllable bursts with a
    /// couple of drifting formants. `step` advances once per tick.
    fn synth_amp(step: usize) -> f32 {
        let s = step as f32;
        let syll = ((s * 0.55).sin() * 0.5 + 0.5).powf(1.6);
        let phrase = ((s * 0.09).sin() * 0.5 + 0.5).max(0.15);
        (syll * phrase * 1.15).clamp(0.0, 1.0)
    }

    /// Band levels in dB for the test signal: a -80 dB room, speech up to
    /// about -25 dB around two drifting formants.
    fn synth_db(step: usize) -> [f32; SPEC_ROWS] {
        let a = synth_amp(step);
        let s = step as f32;
        let k = SPEC_ROWS as f32 / 10.0;
        let f1 = (2.0 + (s * 0.07).sin() * 1.2) * k;
        let f2 = (5.5 + (s * 0.05).cos() * 1.8) * k;
        let mut col = [0.0; SPEC_ROWS];
        for (r, v) in col.iter_mut().enumerate() {
            let r = r as f32;
            let e = (-(r - f1).powi(2) / (2.0 * k * k)).exp()
                + 0.7 * (-(r - f2).powi(2) / (3.0 * k * k)).exp();
            *v = -80.0 + 55.0 * (a * (0.25 + 0.85 * e)).clamp(0.0, 1.0);
        }
        col
    }

    /// How long a transcript stays up: time to read it at adult reading
    /// speed, so a short phrase clears fast and a long one stands.
    fn linger_secs(text: &str) -> f64 {
        let words = text.split_whitespace().count() as f64;
        (LINGER_BASE + words * SECS_PER_WORD).clamp(LINGER_MIN, LINGER_MAX)
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

        // Demo modes, to check the look without dictating:
        //   --demo "<text>"            text panel
        //   --demo-rec dots|spectrum   recording panel with a test signal
        //   --snapshot <file.png>      render to PNG and exit (else show 4 s)
        //   --checker                  composite the PNG over a checkerboard
        let args: Vec<String> = std::env::args().collect();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let checker = args.iter().any(|a| a == "--checker");

        // --probe-mic <secs>: print what the spectrum sees from the default
        // input, twice a second (diagnostic; no window).
        if let Some(secs) = flag("--probe-mic").and_then(|s| s.parse::<f64>().ok()) {
            let Some(m) = Mic::open() else {
                eprintln!("probe: no input device");
                return;
            };
            let mut s = Spectro::new();
            let mut tick = 0u32;
            let end = Instant::now() + Duration::from_secs_f64(secs);
            while Instant::now() < end {
                std::thread::sleep(Duration::from_secs_f64(TICK));
                let Some(samples) = m.latest() else { continue };
                let raw = bands(&samples, m.rate);
                s.push(raw);
                tick += 1;
                if tick % 15 == 0 {
                    let loud = raw.iter().copied().fold(f32::MIN, f32::max);
                    let lit = s.cols.back().map_or(0, |c| c.iter().filter(|&&t| t > 0.0).count());
                    let max_t = s.cols.back().map_or(0.0, |c| c.iter().copied().fold(0.0, f32::max));
                    println!(
                        "t={:.1}s loudest_band={loud:.1}dB floor~{:.1}dB shown={} voiced={} lit_bands={lit} max_t={max_t:.2}",
                        tick as f64 * TICK,
                        s.floor.iter().sum::<f32>() / SPEC_ROWS as f32,
                        s.shown,
                        s.gate.voiced
                    );
                }
            }
            return;
        }
        let demo_text = flag("--demo");
        let demo_rec = flag("--demo-rec").map(|v| Viz::parse(&v));
        if demo_text.is_some() || demo_rec.is_some() {
            let mut step = 0usize;
            let mut amps: VecDeque<f32> = VecDeque::from(vec![0.0; DOT_COLS]);
            let mut spectro = Spectro::new();
            let mut frame = |o: &mut Overlay| {
                if let Some(viz) = demo_rec {
                    step += 1;
                    match viz {
                        Viz::Dots => {
                            amps.pop_front();
                            amps.push_back(synth_amp(step * 2));
                            draw_dots(&mut o.canvas, amps.make_contiguous());
                        }
                        Viz::Spectrum => {
                            spectro.push(synth_db(step));
                            draw_spectrum(&mut o.canvas, &spectro.cols);
                        }
                    }
                    let secs = 3 + step / 30;
                    o.show_recording(&format!("{}:{:02}", secs / 60, secs % 60));
                } else if let Some(t) = &demo_text {
                    o.show_text(&t.replace("\\n", "\n"));
                }
            };
            // Fill the history so a snapshot shows a full visualizer.
            for _ in 0..SPEC_COLS.max(DOT_COLS) {
                frame(&mut overlay);
            }
            while overlay.alpha < 1.0 {
                overlay.animate(true);
            }
            pump(&app, 0.3, || {});
            match flag("--snapshot") {
                Some(path) => overlay.snapshot(&path, checker),
                None => {
                    let o = &mut overlay;
                    let end = Instant::now() + Duration::from_secs(4);
                    while Instant::now() < end {
                        pump(&app, TICK, || {});
                        frame(o);
                    }
                }
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
        let mut rec_start: Option<Instant> = None;
        let mut viz = Viz::Dots;
        let mut mic: Option<Mic> = None;
        let mut spectro = Spectro::new();
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
                            let secs = linger_secs(&shown);
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

            match state.as_str() {
                "recording" | "streaming" => {
                    linger = None;
                    let local = *rec_start.get_or_insert_with(|| {
                        levels.reset();
                        spectro = Spectro::new();
                        viz = Viz::configured();
                        if viz == Viz::Spectrum {
                            mic = Mic::open();
                            if mic.is_none() {
                                eprintln!("voxtype-osd-macos: no input for spectrum, using dots");
                            }
                        }
                        Instant::now()
                    });
                    let start_ms =
                        read_json(&session_path).and_then(|v| v["started_at_ms"].as_u64());
                    let elapsed = match start_ms {
                        Some(ms) => now_ms().saturating_sub(ms) / 1000,
                        None => local.elapsed().as_secs(),
                    };
                    // Hidden until speech starts; fades after a pause.
                    let speaking = match &mic {
                        Some(m) => {
                            if let Some(samples) = m.latest() {
                                spectro.push(bands(&samples, m.rate));
                            }
                            draw_spectrum(&mut overlay.canvas, &spectro.cols);
                            spectro.shown
                        }
                        None => {
                            levels.poll();
                            draw_dots(&mut overlay.canvas, &levels.amps);
                            levels.shown
                        }
                    };
                    overlay.show_recording(&format!("{}:{:02}", elapsed / 60, elapsed % 60));
                    overlay.animate(speaking);
                    continue;
                }
                "transcribing" => {
                    rec_start = None;
                    mic = None;
                    overlay.show_text("Transcribing…");
                    overlay.animate(true);
                }
                _ => {
                    rec_start = None;
                    mic = None;
                    match &linger {
                        Some(l) if Instant::now() < l.until => {
                            let t = l.text.clone();
                            overlay.show_text(&t);
                            overlay.animate(true);
                        }
                        _ => {
                            linger = None;
                            overlay.animate(false);
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fft_finds_a_pure_tone_band() {
            let rate = 48_000.0;
            let tone = 1000.0;
            let s: Vec<f32> = (0..FFT_N)
                .map(|i| 0.5 * (2.0 * std::f32::consts::PI * tone * i as f32 / rate).sin())
                .collect();
            let b = bands(&s, rate);
            let loudest = b
                .iter()
                .enumerate()
                .max_by(|x, y| x.1.total_cmp(y.1))
                .unwrap()
                .0;
            // 1 kHz sits in band floor(24 * ln(1000/80) / ln(100)) = 13.
            assert_eq!(loudest, 13, "bands: {b:?}");
            // Amplitude 0.5 is -6 dBFS; allow Hann scalloping.
            assert!(b[loudest] > -9.0 && b[loudest] < -4.0, "bands: {b:?}");
        }

        fn room() -> [f32; SPEC_ROWS] {
            [-85.0; SPEC_ROWS]
        }

        /// Speech-like frame: a loud band, a band 20 dB quieter, rest room.
        fn voice() -> [f32; SPEC_ROWS] {
            let mut f = room();
            f[6] = -30.0;
            f[7] = -32.0;
            f[8] = -34.0;
            f[9] = -36.0;
            f[10] = -38.0;
            f[11] = -40.0;
            f[12] = -42.0;
            f[13] = -44.0;
            f[16] = -50.0;
            f
        }

        #[test]
        fn hidden_until_speech_then_fades_after_a_pause() {
            let mut s = Spectro::new();
            for i in 0..60 {
                assert!(!s.push(room()), "shown during silence at tick {i}");
            }
            assert!(s.cols.iter().all(|c| c.iter().all(|&t| t == 0.0)), "silence drew something");
            let shown_at = (0..10).find(|_| s.push(voice())).expect("speech never showed");
            assert!(shown_at <= 4, "onset took {shown_at} ticks");
            // A short pause between words keeps the panel up but draws blank.
            for _ in 0..10 {
                assert!(s.push(room()));
            }
            assert!(s.cols.back().unwrap().iter().all(|&t| t == 0.0));
            // A long pause hides it.
            let hidden = (0..60).any(|_| !s.push(room()));
            assert!(hidden, "panel never hid after a long pause");
        }

        #[test]
        fn short_text_clears_fast_long_text_stands() {
            assert_eq!(linger_secs("Okay."), LINGER_MIN);
            let ten = "one two three four five six seven eight nine ten";
            assert!((linger_secs(ten) - 3.5).abs() < 1e-9);
            let long = "word ".repeat(60);
            assert_eq!(linger_secs(&long), LINGER_MAX);
            assert!(linger_secs("a b c") < linger_secs(ten));
        }

        #[test]
        fn startup_zeros_and_room_noise_never_show() {
            let mut s = Spectro::new();
            // The mic's first buffers are exact zeros (-180 dB).
            for _ in 0..5 {
                assert!(!s.push([-180.0; SPEC_ROWS]));
            }
            // Then a fluctuating room: +/- 5 dB per band, no speech.
            for i in 0..150 {
                let mut f = room();
                for (b, v) in f.iter_mut().enumerate() {
                    *v += 5.0 * ((i * 7 + b * 13) as f32 * 0.9).sin();
                }
                assert!(!s.push(f), "room noise showed at tick {i}");
            }
        }

        #[test]
        fn loudest_reads_yellow_and_quieter_reads_purple() {
            let mut s = Spectro::new();
            for _ in 0..30 {
                s.push(room());
            }
            for _ in 0..10 {
                s.push(voice());
            }
            let col = *s.cols.back().unwrap();
            assert!(col[6] > 0.9, "loudest band t={}", col[6]);
            assert!(col[16] > 0.0 && col[16] < 0.7, "quiet band t={}", col[16]);
            assert_eq!(col[0], 0.0, "room-level band drew");
            // Pitch moves: shift the voice up and the lit rows follow.
            let mut high = room();
            high[18] = -30.0;
            high[19] = -32.0;
            for _ in 0..10 {
                s.push(high);
            }
            let col = *s.cols.back().unwrap();
            assert!(col[18] > 0.8 && col[6] == 0.0, "col={col:?}");
        }

        #[test]
        fn spectrum_has_no_gaps() {
            let cols: VecDeque<[f32; SPEC_ROWS]> =
                (0..SPEC_COLS).map(|_| [0.5f32; SPEC_ROWS]).collect();
            let mut c = Canvas::new();
            draw_spectrum(&mut c, &cols);
            let fade_end = (PX_W as f32 * 0.12).ceil() as usize;
            for y in 0..PX_H {
                for x in fade_end..PX_W {
                    let a = c.px[(y * PX_W + x) * 4 + 3];
                    assert!(a >= 240, "seam at x={x} y={y}: alpha {a}");
                }
            }
        }

        #[test]
        fn heat_runs_purple_to_yellow() {
            let lo = heat(0.0);
            let hi = heat(1.0);
            assert!(lo.2 > lo.0 && lo.2 > lo.1, "low end is purple/blue-heavy");
            assert!(hi.0 > 0.9 && hi.1 > 0.8 && hi.2 < 0.5, "high end is yellow");
        }

        #[test]
        fn viz_parses_names() {
            assert_eq!(Viz::parse("spectrum"), Viz::Spectrum);
            assert_eq!(Viz::parse("Spectrogram"), Viz::Spectrum);
            assert_eq!(Viz::parse("dots"), Viz::Dots);
            assert_eq!(Viz::parse("anything"), Viz::Dots);
        }
    }
}
