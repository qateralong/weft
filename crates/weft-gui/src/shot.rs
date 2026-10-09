//! Draws the window into an image without a display, for checking the layout.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError, Rgb8Pixel};

struct Offscreen(Rc<MinimalSoftwareWindow>);

/// The time animations see, moved forward by hand so they finish before the picture is taken.
static CLOCK: AtomicU64 = AtomicU64::new(0);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }

    fn duration_since_start(&self) -> Duration {
        Duration::from_millis(CLOCK.load(Ordering::Relaxed))
    }
}

fn advance(ms: u64) {
    CLOCK.fetch_add(ms, Ordering::Relaxed);
    slint::platform::update_timers_and_animations();
}

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
}

pub fn install(width: u32, height: u32) {
    let window = WINDOW.with(Rc::clone);
    window.set_size(PhysicalSize::new(width, height));
    slint::platform::set_platform(Box::new(Offscreen(window))).expect("no platform set yet");
}

/// Clicks at a point of the window, as a user would.
pub fn click(x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let window = WINDOW.with(Rc::clone);
    let position = slint::LogicalPosition::new(x, y);
    advance(1000);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    window.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    advance(50);
}

/// Renders the current frame to a binary PPM file.
pub fn save(ui: &impl ComponentHandle, path: &Path) -> Result<(), PlatformError> {
    ui.show()?;
    let window = WINDOW.with(Rc::clone);
    let size = window.size();
    let (width, height) = (size.width as usize, size.height as usize);
    let mut pixels = vec![Rgb8Pixel::default(); width * height];
    // Let layouts and animations settle before drawing.
    for _ in 0..3 {
        advance(1000);
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
    }
    let mut out = format!("P6 {width} {height} 255\n").into_bytes();
    for pixel in &pixels {
        out.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(path, out).map_err(|error| PlatformError::Other(error.to_string()))
}
