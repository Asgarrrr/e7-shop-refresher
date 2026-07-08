use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use image::{GrayImage, RgbaImage};

use crate::capture::{Capture, WindowRect};
use crate::config::Config;
use crate::detector::Detector;
use crate::error::{Error, Result};
use crate::input::Input;

use super::ShopRunner;

/// Returns `WindowGone` once frames empty so tests can observe
/// reattach / graceful exit.
pub(super) struct FakeCapture {
    frames: StdMutex<Vec<GrayImage>>,
    /// Override queue for `snapshot_rgba`. Empty → synthesize from the
    /// gray queue like the trait default would.
    rgba_frames: StdMutex<Vec<RgbaImage>>,
    rect: WindowRect,
}

impl FakeCapture {
    pub fn new(frames: Vec<GrayImage>, rect: WindowRect) -> Self {
        Self::with_rgba(frames, vec![], rect)
    }

    pub fn with_rgba(frames: Vec<GrayImage>, rgba: Vec<RgbaImage>, rect: WindowRect) -> Self {
        // Reversed so pop() returns frames in caller-supplied order.
        let frames: Vec<_> = frames.into_iter().rev().collect();
        let rgba: Vec<_> = rgba.into_iter().rev().collect();
        Self {
            frames: StdMutex::new(frames),
            rgba_frames: StdMutex::new(rgba),
            rect,
        }
    }
}

impl Capture for FakeCapture {
    fn snapshot_gray(&self) -> Result<GrayImage> {
        self.frames
            .lock()
            .expect("frames mutex poisoned")
            .pop()
            .ok_or(Error::WindowGone)
    }
    fn snapshot_rgba(&self) -> Result<RgbaImage> {
        if let Some(f) = self.rgba_frames.lock().expect("rgba mutex poisoned").pop() {
            return Ok(f);
        }
        let gray = self.snapshot_gray()?;
        let mut rgba = RgbaImage::new(gray.width(), gray.height());
        for (x, y, p) in gray.enumerate_pixels() {
            let v = p[0];
            rgba.put_pixel(x, y, image::Rgba([v, v, v, 255]));
        }
        Ok(rgba)
    }
    fn rect(&self) -> Result<WindowRect> {
        Ok(self.rect)
    }
    fn check_size_stable(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn restore_to_baseline(&self) -> Result<bool> {
        Ok(true)
    }
    fn local_to_screen(&self, x: i32, y: i32) -> Result<(i32, i32)> {
        Ok((self.rect.x + x, self.rect.y + y))
    }
    fn is_foreground(&self) -> bool {
        true
    }
    fn try_bring_foreground(&self) -> bool {
        true
    }
    fn hwnd_is_valid(&self) -> bool {
        true
    }
    fn reattach(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum FakeEvent {
    Click([i32; 4]),
    Scroll { x: i32, y: i32, lines: i32 },
    HumanPause,
    PauseMs(u64),
    InterRound,
    LongPause,
}

/// Event log behind Arc<Mutex<…>> so tests can inspect after the
/// boxed input has been moved into a runner.
#[derive(Default, Clone)]
pub(super) struct FakeInput {
    pub events: Arc<StdMutex<Vec<FakeEvent>>>,
}

impl Input for FakeInput {
    fn click_local_in_rect(&mut self, _: &dyn Capture, rect: [i32; 4]) -> Result<()> {
        self.events.lock().unwrap().push(FakeEvent::Click(rect));
        Ok(())
    }
    fn scroll_at(&mut self, _: &dyn Capture, x: i32, y: i32, lines: i32) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(FakeEvent::Scroll { x, y, lines });
        Ok(())
    }
    fn human_pause(&mut self) {
        self.events.lock().unwrap().push(FakeEvent::HumanPause);
    }
    fn pause_ms(&self, ms: u64) {
        self.events.lock().unwrap().push(FakeEvent::PauseMs(ms));
    }
    fn inter_round_pause(&mut self) {
        self.events.lock().unwrap().push(FakeEvent::InterRound);
    }
    fn long_human_pause(&mut self) {
        self.events.lock().unwrap().push(FakeEvent::LongPause);
    }
}

pub(super) fn gray_frame(w: u32, h: u32, base: u8) -> GrayImage {
    GrayImage::from_pixel(w, h, image::Luma([base]))
}

pub(super) fn paint_zone(img: &mut GrayImage, [zx, zy, zw, zh]: [f32; 4], value: u8) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let x0 = (zx * w) as u32;
    let y0 = (zy * h) as u32;
    let x1 = ((zx + zw) * w).min(w) as u32;
    let y1 = ((zy + zh) * h).min(h) as u32;
    for y in y0..y1 {
        for x in x0..x1 {
            img.put_pixel(x, y, image::Luma([value]));
        }
    }
}

pub(super) const REFRESH: [f32; 4] = [0.1, 0.8, 0.1, 0.1];
pub(super) const REFRESH_CONFIRM: [f32; 4] = [0.4, 0.5, 0.1, 0.1];
pub(super) const SHOP_GRID: [f32; 4] = [0.1, 0.1, 0.6, 0.6];

/// RGBA frame whose `zone` contains the bundled mystic-medal art —
/// classifies as `mystic_medal` in colour checks; the rest is dark.
pub(super) fn rgba_frame_with_mystic_in(w: u32, h: u32, zone: [f32; 4]) -> RgbaImage {
    let mut img = RgbaImage::from_pixel(w, h, image::Rgba([20, 20, 28, 255]));
    let icon = image::load_from_memory(include_bytes!("../../assets/mystic_medal.png"))
        .expect("bundled asset decodes")
        .into_rgba8();
    let zx = (zone[0] * w as f32) as u32;
    let zy = (zone[1] * h as f32) as u32;
    let zw = ((zone[2] * w as f32) as u32).max(1);
    let zh = ((zone[3] * h as f32) as u32).max(1);
    let icon = image::imageops::resize(&icon, zw, zh, image::imageops::FilterType::Triangle);
    image::imageops::overlay(&mut img, &icon, i64::from(zx), i64::from(zy));
    img
}

pub(super) fn runner_for_loop_tests(
    frames: Vec<GrayImage>,
) -> (ShopRunner, Arc<StdMutex<Vec<FakeEvent>>>) {
    runner_with_frames(frames, vec![])
}

pub(super) fn runner_with_frames(
    frames: Vec<GrayImage>,
    rgba: Vec<RgbaImage>,
) -> (ShopRunner, Arc<StdMutex<Vec<FakeEvent>>>) {
    let mut config: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
    config.zones.refresh = Some(REFRESH);
    config.zones.refresh_confirm = Some(REFRESH_CONFIRM);
    config.zones.buy_confirm = Some([0.4, 0.5, 0.1, 0.1]);
    config.zones.buy_column = Some([0.8, 0.0, 0.1, 1.0]);
    config.regions.shop_grid = Some(SHOP_GRID);
    config.shop.buy_mystic_medals = false;
    config.shop.buy_covenant = false;
    config.shop.max_scrolls_per_round = 0;
    config.shop.sleep_when_done = false;

    let capture: Arc<dyn Capture> = Arc::new(FakeCapture::with_rgba(
        frames,
        rgba,
        WindowRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
    ));
    let detector = Arc::new(Detector::from_test_images(std::collections::HashMap::new()));
    let fake_input = FakeInput::default();
    let events = fake_input.events.clone();
    let input: Box<dyn Input> = Box::new(fake_input);
    let stop = Arc::new(AtomicBool::new(false));
    let live_shop = Arc::new(RwLock::new(config.shop.clone()));
    (
        ShopRunner::new(capture, detector, input, config, live_shop, stop),
        events,
    )
}
