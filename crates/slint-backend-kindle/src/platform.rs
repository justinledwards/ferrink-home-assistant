use std::os::fd::AsRawFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::Rgb8Pixel;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{EventLoopProxy, Platform, PlatformError, WindowAdapter, WindowEvent};

use crate::framebuffer::Framebuffer;
use crate::power::{arm_wakealarm, find_wakealarm, suspend_to_mem};
use crate::touch::TouchInput;
use crate::wakeup::{self, KindleEventLoopProxy, Queue, Wakeup};
use crate::{OnWakeCallback, WakeSchedule};

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
    black_and_white: Arc<AtomicBool>,
    full_refresh_requested: Arc<AtomicBool>,
}

impl KindlePlatform {
    pub(crate) fn new(
        wake_schedule: Arc<Mutex<Option<WakeSchedule>>>,
        on_wake: OnWakeCallback,
        black_and_white: Arc<AtomicBool>,
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
            black_and_white,
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

        frame_buffer.fill(0xff);
        frame_buffer.refresh_full();

        let width = frame_buffer.width as usize;
        let mut rgb_buffer = vec![Rgb8Pixel::default(); width * frame_buffer.height as usize];
        let mut gray_buffer = vec![0u8; width];

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

            // Touch activity counts as user interaction, so it resets the
            // suspend countdown
            if file_descriptors[0].revents & libc::POLLIN != 0 {
                last_interaction = Instant::now();
            }

            touch_input.poll(&self.window);
            slint::platform::update_timers_and_animations();

            let black_and_white = self.black_and_white.load(Ordering::Relaxed);
            self.window.draw_if_needed(|renderer| {
                let dirty = renderer.render(&mut rgb_buffer, width);
                let origin = dirty.bounding_box_origin();
                let size = dirty.bounding_box_size();
                let (x0, y0) = (origin.x as usize, origin.y as usize);
                let (w, h) = (size.width as usize, size.height as usize);

                // The E-ink screen only shows grayscale, so turn each RGB pixel into a single gray value.
                // BT.601 luma weights (0.299, 0.587, 0.114) scaled by 256 and bitshifted to devide by 256.
                let gray = &mut gray_buffer[..w];
                for row in 0..h {
                    let start = (y0 + row) * width + x0;
                    let rgb = &rgb_buffer[start..start + w];
                    for (g, p) in gray.iter_mut().zip(rgb.iter()) {
                        let value =
                            ((77 * p.r as u32 + 150 * p.g as u32 + 29 * p.b as u32) >> 8) as u8;
                        // Black-and-white mode forces pure black/white based on threshold
                        *g = if black_and_white {
                            if value < 128 { 0x00 } else { 0xff }
                        } else {
                            value
                        };
                    }
                    frame_buffer.write_line(y0 + row, x0..x0 + w, gray);
                }
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

fn duration_to_ms(d: Duration) -> libc::c_int {
    // Round up to at least 1 ms. A timeout of 0 makes poll skip the wait
    // entirely, which would spin the CPU if a tiny timer kept re-firing.
    d.as_millis().clamp(1, libc::c_int::MAX as u128) as libc::c_int
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
