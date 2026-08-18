//! Passive Kindle magnetic-cover input.

use std::ffi::CString;

const EVENT_SWITCH: u16 = 5;
const SWITCH_LID: u16 = 0;
const HALL_DEVICE_NAME: &[u8] = b"hall_sensor_disp";

#[repr(C)]
struct InputEvent {
    timestamp_seconds: u32,
    timestamp_microseconds: u32,
    kind: u16,
    code: u16,
    value: i32,
}

fn ioctl_read_request(number: libc::c_ulong, size: usize) -> libc::c_ulong {
    (2 << 30)
        | (libc::c_ulong::try_from(size).expect("fixed ioctl buffer length fits c_ulong") << 16)
        | ((b'E' as libc::c_ulong) << 8)
        | number
}

#[cfg(target_env = "musl")]
unsafe fn input_ioctl(
    file_descriptor: libc::c_int,
    request: libc::c_ulong,
    buffer: *mut u8,
) -> libc::c_int {
    let request_bits = u32::try_from(request).expect("Linux input ioctl request fits 32 bits");
    // SAFETY: the caller guarantees that buffer is writable for the size encoded in request.
    unsafe {
        libc::ioctl(
            file_descriptor,
            libc::c_int::from_ne_bytes(request_bits.to_ne_bytes()),
            buffer,
        )
    }
}

#[cfg(not(target_env = "musl"))]
unsafe fn input_ioctl(
    file_descriptor: libc::c_int,
    request: libc::c_ulong,
    buffer: *mut u8,
) -> libc::c_int {
    // SAFETY: the caller guarantees that buffer is writable for the size encoded in request.
    unsafe { libc::ioctl(file_descriptor, request, buffer) }
}

fn decoded_lid_state(kind: u16, code: u16, value: i32) -> Option<bool> {
    if kind != EVENT_SWITCH || code != SWITCH_LID {
        return None;
    }
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

pub(crate) struct CoverInput {
    file_descriptor: libc::c_int,
    closed: bool,
}

impl CoverInput {
    pub(crate) fn open() -> std::io::Result<Option<Self>> {
        for number in 0..32 {
            let path = format!("/dev/input/event{number}");
            let c_path = CString::new(path).expect("generated input path contains no NUL");
            // SAFETY: c_path is NUL-terminated and the returned descriptor is
            // owned by CoverInput or closed before the next candidate.
            let file_descriptor =
                unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if file_descriptor < 0 {
                continue;
            }
            if !Self::is_reviewed_hall_input(file_descriptor) {
                // SAFETY: this candidate descriptor is still uniquely owned here.
                unsafe { libc::close(file_descriptor) };
                continue;
            }
            let closed = match Self::query_closed(file_descriptor) {
                Ok(closed) => closed,
                Err(error) => {
                    // SAFETY: this candidate descriptor is still uniquely owned here.
                    unsafe { libc::close(file_descriptor) };
                    return Err(error);
                }
            };
            return Ok(Some(Self {
                file_descriptor,
                closed,
            }));
        }
        Ok(None)
    }

    fn is_reviewed_hall_input(file_descriptor: libc::c_int) -> bool {
        let mut name = [0_u8; 64];
        // EVIOCGNAME(64).
        // SAFETY: name is a writable fixed-size buffer for the duration of ioctl.
        let name_result = unsafe {
            input_ioctl(
                file_descriptor,
                ioctl_read_request(0x06, name.len()),
                name.as_mut_ptr(),
            )
        };
        if name_result < 0 {
            return false;
        }
        let name_length = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        if &name[..name_length] != HALL_DEVICE_NAME {
            return false;
        }

        let mut switch_bits = [0_u8; 1];
        // EVIOCGBIT(EV_SW, 1).
        // SAFETY: switch_bits is a writable fixed-size buffer for the ioctl.
        let bits_result = unsafe {
            input_ioctl(
                file_descriptor,
                ioctl_read_request(0x20 + libc::c_ulong::from(EVENT_SWITCH), switch_bits.len()),
                switch_bits.as_mut_ptr(),
            )
        };
        bits_result >= 0 && switch_bits[0] & 1 != 0
    }

    fn query_closed(file_descriptor: libc::c_int) -> std::io::Result<bool> {
        let mut switch_bits = [0_u8; 1];
        // EVIOCGSW(1).
        // SAFETY: switch_bits is a writable fixed-size buffer for the ioctl.
        let result = unsafe {
            input_ioctl(
                file_descriptor,
                ioctl_read_request(0x1b, switch_bits.len()),
                switch_bits.as_mut_ptr(),
            )
        };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(switch_bits[0] & 1 != 0)
        }
    }

    pub(crate) const fn fd(&self) -> libc::c_int {
        self.file_descriptor
    }

    pub(crate) const fn is_closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn read_transition(&mut self) -> std::io::Result<Option<bool>> {
        let mut latest = None;
        loop {
            let mut event = InputEvent {
                timestamp_seconds: 0,
                timestamp_microseconds: 0,
                kind: 0,
                code: 0,
                value: 0,
            };
            // SAFETY: event is initialized writable storage of exactly the size passed.
            let bytes_read = unsafe {
                libc::read(
                    self.file_descriptor,
                    (&raw mut event).cast::<libc::c_void>(),
                    std::mem::size_of::<InputEvent>(),
                )
            };
            if bytes_read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(error);
            }
            if bytes_read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Kindle cover input closed",
                ));
            }
            if usize::try_from(bytes_read).ok() != Some(std::mem::size_of::<InputEvent>()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Kindle cover input returned a partial event",
                ));
            }
            if let Some(closed) = decoded_lid_state(event.kind, event.code, event.value)
                && closed != self.closed
            {
                self.closed = closed;
                latest = Some(closed);
            }
        }
        Ok(latest)
    }
}

impl Drop for CoverInput {
    fn drop(&mut self) {
        // SAFETY: CoverInput uniquely owns this descriptor until drop.
        unsafe { libc::close(self.file_descriptor) };
    }
}

#[cfg(test)]
mod tests {
    use super::decoded_lid_state;

    #[test]
    fn only_lid_switch_open_and_close_values_are_accepted() {
        assert_eq!(decoded_lid_state(5, 0, 0), Some(false));
        assert_eq!(decoded_lid_state(5, 0, 1), Some(true));
        for (kind, code, value) in [(5, 0, -1), (5, 0, 2), (5, 1, 1), (1, 0, 1)] {
            assert_eq!(decoded_lid_state(kind, code, value), None);
        }
    }
}
