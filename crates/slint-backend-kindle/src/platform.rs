use std::os::fd::AsRawFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::Rgb8Pixel;
use slint::platform::software_renderer::{
    LineBufferProvider, MinimalSoftwareWindow, RepaintBufferType,
};
use slint::platform::{EventLoopProxy, Platform, PlatformError, WindowAdapter, WindowEvent};

use crate::cover::CoverInput;
use crate::framebuffer::Framebuffer;
use crate::power::{arm_wakealarm, find_wakealarm, suspend_to_mem};
use crate::touch::TouchInput;
use crate::wakeup::{self, KindleEventLoopProxy, Queue, Wakeup};
use crate::{OnCoverStateCallback, OnWakeCallback, RenderBufferMode, WakeSchedule};

// Animations get redrawn at most ~30 fps. E-ink can't keep up with anything
// faster, so quicker wakes would just waste battery.
const ANIMATION_FRAME: Duration = Duration::from_millis(33);

// The dashboard's original Paperwhite design size.  Slint uses logical pixels,
// while the Kindle framebuffer and touch controller report physical pixels.
// Scaling from this reference size keeps the same composition usable on larger
// Kindles such as the Oasis 3 without a device-specific UI fork.
const BASELINE_WIDTH: f32 = 758.0;
const BASELINE_HEIGHT: f32 = 1024.0;

fn dashboard_scale_factor(framebuffer_width: u32, framebuffer_height: u32) -> f32 {
    (framebuffer_width as f32 / BASELINE_WIDTH).min(framebuffer_height as f32 / BASELINE_HEIGHT)
}

pub(crate) struct KindlePlatform {
    pub(crate) window: Rc<MinimalSoftwareWindow>,
    start: Instant,
    queue: Queue,
    wakeup: Wakeup,
    quit_flag: Arc<AtomicBool>,
    pub(crate) wake_schedule: Arc<Mutex<Option<WakeSchedule>>>,
    pub(crate) on_wake: OnWakeCallback,
    pub(crate) on_cover_state: OnCoverStateCallback,
    black_and_white: Arc<AtomicBool>,
    render_buffer_mode: Arc<AtomicU8>,
    full_refresh_requested: Arc<AtomicBool>,
}

impl KindlePlatform {
    pub(crate) fn new(
        wake_schedule: Arc<Mutex<Option<WakeSchedule>>>,
        on_wake: OnWakeCallback,
        on_cover_state: OnCoverStateCallback,
        black_and_white: Arc<AtomicBool>,
        render_buffer_mode: Arc<AtomicU8>,
        full_refresh_requested: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let wakeup = wakeup::make_wakeup()?;
        Ok(Self {
            window,
            start: Instant::now(),
            queue: Arc::new(Mutex::new(Vec::new())),
            wakeup,
            quit_flag: Arc::new(AtomicBool::new(false)),
            wake_schedule,
            on_wake,
            on_cover_state,
            black_and_white,
            render_buffer_mode,
            full_refresh_requested,
        })
    }

    /// Suspend the device to RAM once it's been idle for `stay_awake` with no
    /// pending work, then arm the wakealarm to bring it back. Returns `true`
    /// if a suspend cycle ran (the caller should restart the event loop).
    fn suspend_if_idle(
        &self,
        frame_buffer: &Framebuffer,
        wakealarm: Option<&Path>,
        last_interaction: &mut Instant,
    ) -> bool {
        let (Some(schedule), Some(wakealarm_path)) = (
            *self.wake_schedule.lock().expect("wake schedule poisoned"),
            wakealarm,
        ) else {
            return false;
        };

        // Pending Slint timers don't block suspend: they'll just fire on
        // resume (a 1 Hz clock timer would otherwise pin the device awake).
        let nothing_pending = !self.window.has_active_animations()
            && self
                .queue
                .lock()
                .expect("event loop closure queue poisoned")
                .is_empty();
        if last_interaction.elapsed() < schedule.stay_awake || !nothing_pending {
            return false;
        }

        frame_buffer.wait_for_update_complete();

        // If arming fails we still suspend, sleeping to save battery is better than
        // staying awake.
        if let Err(e) = arm_wakealarm(wakealarm_path, schedule.wake_interval) {
            log::error!(
                "failed to arm RTC wakealarm: {e}; device may only wake on user input this cycle"
            );
        }
        if let Err(e) = suspend_to_mem() {
            log::error!("suspend-to-RAM failed: {e}");
        }

        // Start a fresh stay_awake window so the consumer's app
        // gets at least that long to react.
        *last_interaction = Instant::now();
        // Fire the consumer's on-wake callback (if any) before any rendering
        // this cycle, so e.g. an HTTP poll runs before the next draw shows
        // stale data.
        if let Some(callback) = self.on_wake.borrow_mut().as_mut() {
            callback();
        }
        true
    }
}

impl Platform for KindlePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(KindleEventLoopProxy {
            queue: self.queue.clone(),
            write_fd: self.wakeup.write.clone(),
            quit_flag: self.quit_flag.clone(),
        }))
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let mut frame_buffer = Framebuffer::open()
            .map_err(|e| PlatformError::Other(format!("failed to open /dev/fb0: {e}")))?;

        // The physical framebuffer may be much larger than the dashboard's
        // Paperwhite reference layout.  Tell Slint how to convert the layout's
        // logical pixels to the panel's physical pixels before sizing the window.
        // Keeping the scale uniform prevents text and touch targets from being
        // stretched when a Kindle has a slightly different aspect ratio.
        let scale_factor = dashboard_scale_factor(frame_buffer.width, frame_buffer.height);
        let _ = self
            .window
            .try_dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
        self.window.set_size(slint::PhysicalSize::new(
            frame_buffer.width,
            frame_buffer.height,
        ));

        let mut touch_input =
            TouchInput::open(frame_buffer.width, frame_buffer.height, scale_factor)
                .map_err(|e| PlatformError::Other(format!("failed to open touch input: {e}")))?;
        let mut cover_input = match CoverInput::open() {
            Ok(input) => input,
            Err(error) => {
                log::warn!("failed to inspect Kindle cover input: {error}");
                None
            }
        };
        if let Some(cover) = cover_input.as_ref()
            && let Some(callback) = self.on_cover_state.borrow_mut().as_mut()
        {
            callback(cover.is_closed());
        }

        frame_buffer.fill(0xff);
        frame_buffer.refresh_full();

        let width = frame_buffer.width as usize;
        let height = frame_buffer.height as usize;
        let mut full_frame_buffer: Option<Vec<Rgb8Pixel>> = None;
        let mut line_buffer = vec![Rgb8Pixel::default(); width];
        let mut gray_buffer = vec![0u8; width];
        let mut active_render_mode = RenderBufferMode::load(&self.render_buffer_mode);

        let wakeup_read_fd = self.wakeup.read.as_raw_fd();

        // Wakealarm path is probed once. If the device doesn't expose one
        // (e.g. running on a dev host), the suspend cycle stays disabled even
        // if a schedule is configured.
        let wakealarm = find_wakealarm().ok();
        let mut last_interaction = Instant::now();

        loop {
            // A suspend cycle restarts the loop with a fresh stay-awake window.
            if self.suspend_if_idle(&frame_buffer, wakealarm.as_deref(), &mut last_interaction) {
                continue;
            }

            // Wait for touch event or wakeup from application thread.
            // -1 means "wait forever," which lets the CPU go to sleep.
            let timeout_ms: libc::c_int = match (
                self.window.has_active_animations(),
                slint::platform::duration_until_next_timer_update(),
            ) {
                (true, Some(d)) => duration_to_ms(d.min(ANIMATION_FRAME)),
                (true, None) => duration_to_ms(ANIMATION_FRAME),
                (false, Some(d)) => duration_to_ms(d),
                (false, None) => -1,
            };

            // [0] - touch events file descriptor
            // [1] - wakeup pipe for userland application threads
            // [2] - passive magnetic-cover events, or -1 when unavailable
            let mut file_descriptors = [
                libc::pollfd {
                    fd: touch_input.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wakeup_read_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cover_input.as_ref().map_or(-1, CoverInput::fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            // Block until an fd has activity or the timeout expires.
            // Retry on EINTR, bail on any other error.
            // SAFETY: fds is a valid 2-element array while poll runs.
            let poll_result = unsafe {
                libc::poll(
                    file_descriptors.as_mut_ptr(),
                    file_descriptors.len() as libc::nfds_t,
                    timeout_ms,
                )
            };
            if poll_result < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(PlatformError::Other(format!("poll failed: {err}")));
            }

            // Bail if either file descriptor has died to avoid waiting forever on input
            let err_bits = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if (file_descriptors[0].revents | file_descriptors[1].revents) & err_bits != 0 {
                return Err(PlatformError::Other(format!(
                    "poll: input fd died (touch revents={:#x}, wakeup revents={:#x})",
                    file_descriptors[0].revents, file_descriptors[1].revents
                )));
            }
            if cover_input.is_some() && file_descriptors[2].revents & err_bits != 0 {
                log::warn!(
                    "disabling Kindle cover indicator after poll error {:#x}",
                    file_descriptors[2].revents
                );
                cover_input = None;
            }

            // Empty the pipe before running closures so any new wakeup that arrives
            // while a closure runs still triggers another loop iteration.
            if file_descriptors[1].revents & libc::POLLIN != 0 {
                wakeup::drain(&self.wakeup.read);
                let pending: Vec<_> = self
                    .queue
                    .lock()
                    .expect("event loop closure queue poisoned")
                    .drain(..)
                    .collect();
                for c in pending {
                    c();
                }
            }

            // Check early for quit before doing more work
            if self.quit_flag.load(Ordering::SeqCst) {
                break;
            }

            if file_descriptors[2].revents & libc::POLLIN != 0
                && let Some(cover) = cover_input.as_mut()
            {
                match cover.read_transition() {
                    Ok(Some(closed)) => {
                        if let Some(callback) = self.on_cover_state.borrow_mut().as_mut() {
                            callback(closed);
                        }
                        self.window.request_redraw();
                        self.full_refresh_requested.store(true, Ordering::Release);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        log::warn!("disabling Kindle cover indicator after read error: {error}");
                        cover_input = None;
                    }
                }
            }

            // Touch activity counts as user interaction, so it resets the
            // suspend countdown
            if file_descriptors[0].revents & libc::POLLIN != 0 {
                last_interaction = Instant::now();
            }

            touch_input.poll(&self.window);
            slint::platform::update_timers_and_animations();

            let black_and_white = self.black_and_white.load(Ordering::Relaxed);
            self.window.draw_if_needed(|renderer| {
                let requested_render_mode = RenderBufferMode::load(&self.render_buffer_mode);
                if requested_render_mode != active_render_mode {
                    renderer.set_repaint_buffer_type(RepaintBufferType::NewBuffer);
                    active_render_mode = requested_render_mode;
                }
                let dirty = match requested_render_mode {
                    RenderBufferMode::FullFrame => {
                        let rgb_buffer = full_frame_buffer.get_or_insert_with(|| {
                            vec![Rgb8Pixel::default(); width.saturating_mul(height)]
                        });
                        let dirty = renderer.render(rgb_buffer, width);
                        let origin = dirty.bounding_box_origin();
                        let size = dirty.bounding_box_size();
                        let (x0, y0) = (origin.x as usize, origin.y as usize);
                        let (w, h) = (size.width as usize, size.height as usize);

                        for row in 0..h {
                            let start = (y0 + row) * width + x0;
                            rgb_to_gray(
                                &rgb_buffer[start..start + w],
                                &mut gray_buffer[..w],
                                black_and_white,
                            );
                            frame_buffer.write_line(y0 + row, x0..x0 + w, &gray_buffer[..w]);
                        }
                        dirty
                    }
                    RenderBufferMode::Scanline => {
                        full_frame_buffer = None;
                        renderer.render_by_line(KindleLineBuffer {
                            frame_buffer: &mut frame_buffer,
                            rgb: &mut line_buffer,
                            gray: &mut gray_buffer,
                            black_and_white,
                        })
                    }
                };
                renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
                let origin = dirty.bounding_box_origin();
                let size = dirty.bounding_box_size();
                frame_buffer.refresh_region(origin, size);
            });

            // Run a user-requested cleaning cycle only after Slint's latest
            // frame has reached the framebuffer. That avoids a partial update
            // racing behind the full GC16 update and immediately reintroducing
            // artifacts. Multiple taps before this point collapse into one.
            if self.full_refresh_requested.swap(false, Ordering::AcqRel) {
                frame_buffer.refresh_full();
                frame_buffer.wait_for_update_complete();
            }
        }

        Ok(())
    }
}

struct KindleLineBuffer<'a> {
    frame_buffer: &'a mut Framebuffer,
    rgb: &'a mut [Rgb8Pixel],
    gray: &'a mut [u8],
    black_and_white: bool,
}

impl LineBufferProvider for KindleLineBuffer<'_> {
    type TargetPixel = Rgb8Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: std::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let length = range.len();
        let rgb = &mut self.rgb[..length];
        render_fn(rgb);
        rgb_to_gray(rgb, &mut self.gray[..length], self.black_and_white);
        self.frame_buffer
            .write_line(line, range, &self.gray[..length]);
    }
}

fn rgb_to_gray(rgb: &[Rgb8Pixel], gray: &mut [u8], black_and_white: bool) {
    debug_assert_eq!(rgb.len(), gray.len());
    for (destination, pixel) in gray.iter_mut().zip(rgb) {
        // BT.601 luma weights scaled by 256 for an integer conversion.
        let value = ((77 * u32::from(pixel.r) + 150 * u32::from(pixel.g) + 29 * u32::from(pixel.b))
            >> 8) as u8;
        *destination = if black_and_white {
            if value < 128 { 0x00 } else { 0xff }
        } else {
            value
        };
    }
}

fn duration_to_ms(d: Duration) -> libc::c_int {
    // Round up to at least 1 ms. A timeout of 0 makes poll skip the wait
    // entirely, which would spin the CPU if a tiny timer kept re-firing.
    d.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int
}

#[cfg(test)]
mod pixel_tests {
    use super::*;

    #[test]
    fn rgb_conversion_supports_gray_and_bilevel_output() {
        let rgb = [
            Rgb8Pixel { r: 0, g: 0, b: 0 },
            Rgb8Pixel {
                r: 255,
                g: 255,
                b: 255,
            },
            Rgb8Pixel { r: 255, g: 0, b: 0 },
        ];
        let mut gray = [0; 3];
        rgb_to_gray(&rgb, &mut gray, false);
        assert_eq!(gray, [0, 255, 76]);

        rgb_to_gray(&rgb, &mut gray, true);
        assert_eq!(gray, [0, 255, 0]);
    }
}

#[cfg(test)]
mod tests {
    use super::dashboard_scale_factor;

    #[test]
    fn paperwhite_reference_geometry_is_unscaled() {
        assert_eq!(dashboard_scale_factor(758, 1024), 1.0);
    }

    #[test]
    fn oasis_3_geometry_scales_uniformly_from_height() {
        let scale = dashboard_scale_factor(1264, 1680);
        assert!((scale - 1.640625).abs() < f32::EPSILON);
    }
}
