//! The mirror window: software-scaled presentation, scroll-to-resize, hotkeys
//! and a tiny on-screen indicator for mute / audio reset.

use crate::audio::AudioHandle;
use anyhow::Result;
use std::{
    num::NonZeroU32,
    sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex},
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    window::{Fullscreen, Window, WindowId, WindowLevel},
};

/// One decoded picture, `0x00RRGGBB` pixels.
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

pub enum UserEvent {
    Frame,
    Connected(String),
    Disconnected,
    /// Re-read state that changed outside the window (e.g. mute from the panel).
    Refresh,
    Quit,
}

/// Thread-safe handle the network/decoder threads use to talk to the window.
#[derive(Clone)]
pub struct UiLink {
    proxy: EventLoopProxy<UserEvent>,
    slot: Arc<Mutex<Option<Frame>>>,
    /// Total frames ever published (diagnostics / tests).
    pushed: Arc<AtomicU64>,
}

impl UiLink {
    /// Publish a frame. Only the newest frame is kept: if the window falls
    /// behind, stale frames are dropped instead of building up latency.
    pub fn push_frame(&self, f: Frame) {
        self.pushed.fetch_add(1, Ordering::Relaxed);
        let was_empty = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            let was_empty = slot.is_none();
            *slot = Some(f);
            was_empty
        };
        if was_empty {
            let _ = self.proxy.send_event(UserEvent::Frame);
        }
    }
    pub fn connected(&self, device: String) {
        crate::ipc::emit(&format!("connected {device}"));
        let _ = self.proxy.send_event(UserEvent::Connected(device));
    }
    pub fn refresh(&self) {
        let _ = self.proxy.send_event(UserEvent::Refresh);
    }
    pub fn quit(&self) {
        let _ = self.proxy.send_event(UserEvent::Quit);
    }
    pub fn disconnected(&self) {
        crate::ipc::emit("disconnected");
        let _ = self.proxy.send_event(UserEvent::Disconnected);
    }
}

pub fn create_event_loop() -> Result<(EventLoop<UserEvent>, UiLink)> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let link = UiLink { proxy: event_loop.create_proxy(), slot: Arc::new(Mutex::new(None)), pushed: Arc::new(AtomicU64::new(0)) };
    Ok((event_loop, link))
}

/// A `UiLink` for tests. winit allows one event loop per process, so it is
/// created once on a parked thread and shared by every test.
#[cfg(test)]
pub fn test_link() -> UiLink {
    static LINK: std::sync::OnceLock<UiLink> = std::sync::OnceLock::new();
    LINK.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            let event_loop = EventLoop::<UserEvent>::with_user_event().with_any_thread(true).build().unwrap();
            let link = UiLink {
                proxy: event_loop.create_proxy(),
                slot: Arc::new(Mutex::new(None)),
                pushed: Arc::new(AtomicU64::new(0)),
            };
            tx.send(link).unwrap();
            let _keep_alive = event_loop;
            loop {
                std::thread::park();
            }
        });
        rx.recv().unwrap()
    })
    .clone()
}
#[cfg(test)]
impl UiLink {
    pub fn take_frame_for_tests(&self) -> Option<Frame> {
        self.slot.lock().unwrap().take()
    }
    pub fn pushed_for_tests(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }
}

pub fn run(event_loop: EventLoop<UserEvent>, link: UiLink, name: String, audio: AudioHandle, on_top: bool) -> Result<()> {
    let mut app = App {
        name,
        audio,
        slot: link.slot.clone(),
        window: None,
        surface: None,
        frame: None,
        device: None,
        visible: false,
        last_click: None,
        osd: None,
        fullscreen: false,
        on_top,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

// ------------------------------------------------------------------ app

const OSD_TIME: Duration = Duration::from_millis(1400);
const MIN_SIZE: u32 = 200;

#[derive(Clone, Copy)]
enum Glyph {
    Sound,
    Muted,
    Reset,
    Pin,
}

struct App {
    name: String,
    audio: AudioHandle,
    slot: Arc<Mutex<Option<Frame>>>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    frame: Option<Frame>,
    device: Option<String>,
    visible: bool,
    last_click: Option<Instant>,
    osd: Option<(Glyph, Instant)>,
    fullscreen: bool,
    on_top: bool,
}

impl App {
    fn title(&self) -> String {
        let mut t = match &self.device {
            Some(d) => format!("{d} \u{2014} {}", self.name),
            None => self.name.clone(),
        };
        if self.audio.is_muted() {
            t.push_str("  [muted]");
        }
        t
    }

    fn update_title(&self) {
        if let Some(w) = &self.window {
            w.set_title(&self.title());
        }
    }

    fn flash(&mut self, g: Glyph) {
        self.osd = Some((g, Instant::now()));
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn monitor_size(&self) -> PhysicalSize<u32> {
        self.window
            .as_ref()
            .and_then(|w| w.current_monitor().or_else(|| w.primary_monitor()))
            .map(|m| m.size())
            .unwrap_or(PhysicalSize::new(1920, 1080))
    }

    /// Resize keeping the window centre where it was.
    fn resize_centered(&self, nw: u32, nh: u32) {
        let Some(w) = &self.window else { return };
        let old = w.inner_size();
        let pos = w.outer_position().ok();
        let _ = w.request_inner_size(PhysicalSize::new(nw, nh));
        if let Some(p) = pos {
            let dx = (old.width as i32 - nw as i32) / 2;
            let dy = (old.height as i32 - nh as i32) / 2;
            w.set_outer_position(PhysicalPosition::new(p.x + dx, p.y + dy));
        }
    }

    /// Largest size with the given aspect that fits `limit`.
    fn fit(aspect: f64, w: f64, h: f64, limit: PhysicalSize<u32>) -> (u32, u32) {
        let (lw, lh) = (limit.width as f64 * 0.96, limit.height as f64 * 0.92);
        let k = (lw / w).min(lh / h).min(1.0);
        let (w, h) = (w * k, h * k);
        let _ = aspect;
        (w.round().max(1.0) as u32, h.round().max(1.0) as u32)
    }

    fn on_new_frame(&mut self, f: &Frame) {
        let Some(win) = self.window.clone() else { return };
        let aspect = f.w as f64 / f.h as f64;
        let old_aspect = self.frame.as_ref().map(|o| o.w as f64 / o.h as f64);
        let mon = self.monitor_size();

        if !self.visible {
            // First picture of a session: open at a comfortable size, centred.
            let target_h = (mon.height as f64 * 0.8).min(f.h as f64);
            let (w, h) = Self::fit(aspect, target_h * aspect, target_h, mon);
            let _ = win.request_inner_size(PhysicalSize::new(w, h));
            win.set_outer_position(PhysicalPosition::new(
                (mon.width as i32 - w as i32) / 2,
                (mon.height as i32 - h as i32) / 3,
            ));
            win.set_visible(true);
            win.focus_window();
            self.visible = true;
        } else if old_aspect.is_some_and(|a| (a - aspect).abs() > 0.01) && !self.fullscreen {
            // Device rotated: keep the long edge so the window feels stable.
            let cur = win.inner_size();
            let long = cur.width.max(cur.height) as f64;
            let (w, h) = if aspect >= 1.0 { (long, long / aspect) } else { (long * aspect, long) };
            let (w, h) = Self::fit(aspect, w, h, mon);
            self.resize_centered(w, h);
        }
    }

    fn scroll_resize(&mut self, steps: f64) {
        if self.fullscreen {
            return;
        }
        let (Some(win), Some(f)) = (self.window.clone(), self.frame.as_ref()) else { return };
        let aspect = f.w as f64 / f.h as f64;
        if win.is_maximized() {
            win.set_maximized(false);
        }
        let cur = win.inner_size();
        let mon = self.monitor_size();
        let factor = 1.07f64.powf(steps);

        let mut w = cur.width as f64 * factor;
        let mut h = w / aspect;
        // clamp: not smaller than MIN_SIZE on the short edge, not larger than the monitor
        let min_k = (MIN_SIZE as f64 / w).max(MIN_SIZE as f64 / h);
        if min_k > 1.0 {
            w *= min_k;
            h *= min_k;
        }
        let (mw, mh) = (mon.width as f64 * 0.98, mon.height as f64 * 0.94);
        let max_k = (mw / w).min(mh / h);
        if max_k < 1.0 {
            w *= max_k;
            h *= max_k;
        }
        let (w, h) = (w.round() as u32, h.round() as u32);
        if (w, h) != (cur.width, cur.height) {
            self.resize_centered(w, h);
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(w) = &self.window else { return };
        self.fullscreen = !self.fullscreen;
        w.set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
        w.set_cursor_visible(true);
    }

    fn redraw(&mut self) {
        let (Some(win), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else { return };
        let size = win.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        match &self.frame {
            Some(f) => blit_fit(&mut buf, size.width as usize, size.height as usize, f),
            None => buf.fill(0),
        }
        if let Some((glyph, at)) = self.osd {
            let left = OSD_TIME.saturating_sub(at.elapsed());
            if left.is_zero() {
                self.osd = None;
            } else {
                let alpha = (left.as_millis() as f32 / 350.0).min(1.0);
                draw_osd(&mut buf, size.width as usize, size.height as usize, glyph, alpha, win.scale_factor());
            }
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.title())
            .with_window_icon(winit::window::Icon::from_rgba(crate::icon::rgba(256), 256, 256).ok())
            .with_visible(false)
            .with_window_level(if self.on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal })
            .with_inner_size(PhysicalSize::new(390, 844))
            .with_min_inner_size(PhysicalSize::new(MIN_SIZE / 2, MIN_SIZE / 2));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("cannot create window: {e}");
                el.exit();
                return;
            }
        };
        match softbuffer::Context::new(window.clone())
            .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
        {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                log::error!("cannot create drawing surface: {e}");
                el.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::Frame => {
                let f = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(f) = f {
                    self.on_new_frame(&f);
                    self.frame = Some(f);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            UserEvent::Refresh => self.update_title(),
            UserEvent::Quit => el.exit(),
            UserEvent::Connected(d) => {
                self.device = Some(d);
                self.update_title();
            }
            UserEvent::Disconnected => {
                self.device = None;
                self.frame = None;
                self.osd = None;
                self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(w) = &self.window {
                    if self.fullscreen {
                        self.fullscreen = false;
                        w.set_fullscreen(None);
                    }
                    w.set_visible(false);
                }
                self.visible = false;
                self.update_title();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 60.0,
                };
                if steps != 0.0 {
                    self.scroll_resize(steps);
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let now = Instant::now();
                if self.last_click.is_some_and(|t| now.duration_since(t) < Duration::from_millis(350)) {
                    self.last_click = None;
                    self.toggle_fullscreen();
                } else {
                    self.last_click = Some(now);
                    if !self.fullscreen {
                        if let Some(w) = &self.window {
                            let _ = w.drag_window();
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                match &event.logical_key {
                    Key::Character(c) => match c.to_lowercase().as_str() {
                        "m" => {
                            let muted = self.audio.toggle_mute();
                            self.update_title();
                            self.flash(if muted { Glyph::Muted } else { Glyph::Sound });
                            log::info!("audio {}", if muted { "muted" } else { "unmuted" });
                            crate::ipc::emit(&format!("muted {}", muted as u8));
                        }
                        "r" => {
                            self.audio.reset();
                            self.flash(Glyph::Reset);
                        }
                        "f" => self.toggle_fullscreen(),
                        "t" => {
                            self.on_top = !self.on_top;
                            if let Some(w) = &self.window {
                                w.set_window_level(if self.on_top {
                                    WindowLevel::AlwaysOnTop
                                } else {
                                    WindowLevel::Normal
                                });
                            }
                            self.flash(Glyph::Pin);
                        }
                        _ => {}
                    },
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    Key::Named(NamedKey::Escape) if self.fullscreen => self.toggle_fullscreen(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // keep the indicator animating (fade-out) even when no new frames arrive
        match self.osd {
            Some((_, at)) if at.elapsed() < OSD_TIME => {
                el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(30)));
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Some(_) => {
                self.osd = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

// ------------------------------------------------------------ rendering

const MASK_RB: u32 = 0x00FF_00FF;

#[inline]
fn lerp(a: u32, b: u32, t: u32) -> u32 {
    // t in 0..=256
    let rb = (((a & MASK_RB) * (256 - t) + (b & MASK_RB) * t) >> 8) & MASK_RB;
    let g = ((((a >> 8) & 0xFF) * (256 - t) + ((b >> 8) & 0xFF) * t) >> 8) & 0xFF;
    rb | (g << 8)
}

/// Scale `src` into the largest centred, aspect-correct rectangle of the
/// `dw` x `dh` buffer (black bars elsewhere). Downscaling uses a box filter,
/// upscaling bilinear.
fn blit_fit(dst: &mut [u32], dw: usize, dh: usize, src: &Frame) {
    let (sw, sh) = (src.w as usize, src.h as usize);
    let k = (dw as f64 / sw as f64).min(dh as f64 / sh as f64);
    let rw = ((sw as f64 * k).round() as usize).clamp(1, dw);
    let rh = ((sh as f64 * k).round() as usize).clamp(1, dh);
    let (ox, oy) = ((dw - rw) / 2, (dh - rh) / 2);

    if rw != dw || rh != dh {
        dst.fill(0);
    }

    let upscale = rw >= sw;
    // per-column source ranges / weights
    struct Col {
        a: usize,
        b: usize,
        t: u32,
    }
    let cols: Vec<Col> = (0..rw)
        .map(|x| {
            if upscale {
                let s = ((x as f64 + 0.5) * sw as f64 / rw as f64 - 0.5).max(0.0);
                let a = (s.floor() as usize).min(sw - 1);
                Col { a, b: (a + 1).min(sw - 1), t: ((s - a as f64) * 256.0) as u32 }
            } else {
                let a = x * sw / rw;
                Col { a, b: ((x + 1) * sw / rw).max(a + 1).min(sw), t: 0 }
            }
        })
        .collect();

    let rows = &mut dst[oy * dw..(oy + rh) * dw];
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
    let per = rh.div_ceil(if rh * rw > 200_000 { threads } else { 1 });

    std::thread::scope(|scope| {
        for (ci, chunk) in rows.chunks_mut(per * dw).enumerate() {
            let cols = &cols;
            scope.spawn(move || {
                for (ry, line) in chunk.chunks_mut(dw).enumerate() {
                    let y = ci * per + ry;
                    let line = &mut line[ox..ox + rw];
                    if upscale {
                        let s = ((y as f64 + 0.5) * sh as f64 / rh as f64 - 0.5).max(0.0);
                        let y0 = (s.floor() as usize).min(sh - 1);
                        let y1 = (y0 + 1).min(sh - 1);
                        let ty = ((s - y0 as f64) * 256.0) as u32;
                        let (r0, r1) = (&src.px[y0 * sw..(y0 + 1) * sw], &src.px[y1 * sw..(y1 + 1) * sw]);
                        for (o, c) in line.iter_mut().zip(cols) {
                            let top = lerp(r0[c.a], r0[c.b], c.t);
                            let bot = lerp(r1[c.a], r1[c.b], c.t);
                            *o = lerp(top, bot, ty);
                        }
                    } else {
                        let y0 = y * sh / rh;
                        let y1 = ((y + 1) * sh / rh).max(y0 + 1).min(sh);
                        for (o, c) in line.iter_mut().zip(cols) {
                            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                            for yy in y0..y1 {
                                for &p in &src.px[yy * sw + c.a..yy * sw + c.b] {
                                    r += (p >> 16) & 0xFF;
                                    g += (p >> 8) & 0xFF;
                                    b += p & 0xFF;
                                }
                            }
                            let n = ((y1 - y0) * (c.b - c.a)) as u32;
                            *o = ((r / n) << 16) | ((g / n) << 8) | (b / n);
                        }
                    }
                }
            });
        }
    });
}

// ------------------------------------------------------------ indicator

const SPEAKER: [&str; 16] = [
    "................",
    "................",
    ".......#........",
    "......##...#....",
    "....####....#...",
    "....####..#..#..",
    "....####...#.#..",
    "....####...#.#..",
    "....####..#..#..",
    "....####....#...",
    "......##...#....",
    ".......#........",
    "................",
    "................",
    "................",
    "................",
];

const SPEAKER_MUTED: [&str; 16] = [
    "................",
    "................",
    ".......#........",
    "......##........",
    "....####........",
    "....####.#...#..",
    "....####..#.#...",
    "....####...#....",
    "....####..#.#...",
    "....####.#...#..",
    "......##........",
    ".......#........",
    "................",
    "................",
    "................",
    "................",
];

const PIN: [&str; 16] = [
    "................",
    "....########....",
    ".....#....#.....",
    ".....#....#.....",
    ".....#....#.....",
    "....########....",
    "...#........#...",
    "................",
    ".......##.......",
    ".......##.......",
    ".......##.......",
    ".......##.......",
    ".......##.......",
    "................",
    "................",
    "................",
];

fn glyph_on(g: Glyph, x: usize, y: usize) -> bool {
    match g {
        Glyph::Sound => SPEAKER[y].as_bytes()[x] == b'#',
        Glyph::Muted => SPEAKER_MUTED[y].as_bytes()[x] == b'#',
        Glyph::Pin => PIN[y].as_bytes()[x] == b'#',
        Glyph::Reset => {
            // circular arrow: a ring with a gap at the top right, plus an arrow head
            let (dx, dy) = (x as f32 - 7.5, y as f32 - 7.5);
            let r = (dx * dx + dy * dy).sqrt();
            let ang = dy.atan2(dx);
            let ring = (4.6..=6.6).contains(&r) && !(-1.35..=-0.35).contains(&ang);
            let head = {
                let (px, py) = (x as f32, y as f32);
                // triangle (9,0)-(14,3)-(9,6) pointing clockwise at the gap
                let d = |ax: f32, ay: f32, bx: f32, by: f32| (px - bx) * (ay - by) - (ax - bx) * (py - by);
                let (d1, d2, d3) = (d(9.0, 0.0, 14.0, 3.0), d(14.0, 3.0, 9.0, 6.0), d(9.0, 6.0, 9.0, 0.0));
                !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
            };
            ring || head
        }
    }
}

/// Rounded dark plate with a white glyph in the top-right corner.
fn draw_osd(dst: &mut [u32], dw: usize, dh: usize, g: Glyph, alpha: f32, scale_factor: f64) {
    let unit = ((4.0 * scale_factor).round() as usize).max(2);
    let plate = unit * 16 + unit * 4;
    let margin = unit * 5;
    if plate + margin >= dw || plate + margin >= dh {
        return;
    }
    let (x0, y0) = (dw - plate - margin, margin);
    let radius = (plate / 5) as f32;
    let bg_a = 0.62 * alpha;
    let fg_a = 0.95 * alpha;

    for py in 0..plate {
        for px in 0..plate {
            // rounded-rect mask
            let cx = (px as f32 + 0.5 - plate as f32 / 2.0).abs() - (plate as f32 / 2.0 - radius);
            let cy = (py as f32 + 0.5 - plate as f32 / 2.0).abs() - (plate as f32 / 2.0 - radius);
            if cx.max(0.0).hypot(cy.max(0.0)) > radius {
                continue;
            }
            let (gx, gy) = (px as isize - (unit * 2) as isize, py as isize - (unit * 2) as isize);
            let on = gx >= 0
                && gy >= 0
                && (gx as usize) < unit * 16
                && (gy as usize) < unit * 16
                && glyph_on(g, gx as usize / unit, gy as usize / unit);
            let (target, a) = if on { (255.0, fg_a) } else { (12.0, bg_a) };
            let o = &mut dst[(y0 + py) * dw + x0 + px];
            let mix = |c: u32| (c as f32 * (1.0 - a) + target * a) as u32;
            *o = (mix((*o >> 16) & 0xFF) << 16) | (mix((*o >> 8) & 0xFF) << 8) | mix(*o & 0xFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_preserves_flat_colour_and_letterboxes() {
        let src = Frame { w: 4, h: 4, px: vec![0x00336699; 16] };
        // wider target: 4:4 picture centred in 12x6, bars on the sides
        let mut dst = vec![1u32; 12 * 6];
        blit_fit(&mut dst, 12, 6, &src);
        assert!(dst[0] == 0 && dst[11] == 0, "side bars are black");
        assert!(dst[3 * 12 + 6] == 0x00336699, "centre keeps the colour");
        // downscale path
        let big = Frame { w: 64, h: 64, px: vec![0x00FF8000; 64 * 64] };
        let mut dst = vec![0u32; 16 * 16];
        blit_fit(&mut dst, 16, 16, &big);
        assert!(dst.iter().all(|&p| p == 0x00FF8000));
    }
}