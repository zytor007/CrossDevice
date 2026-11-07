use cd_geometry::MonitorInfo;
use cd_proto::{InputEvent, InputKind};

use crate::error::InputError;
use crate::x11::{CapturedEvent, X11Injector};

/// Converts captured X11 events into C1 events using the physical density of
/// the source monitor. The first motion event establishes the origin.
#[derive(Debug, Clone)]
pub struct PointerMapper {
    monitor: MonitorInfo,
    last: Option<(f64, f64)>,
}

impl PointerMapper {
    pub fn new(monitor: MonitorInfo) -> Self {
        Self {
            monitor,
            last: None,
        }
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn map(&mut self, event: &CapturedEvent) -> Option<InputEvent> {
        match event {
            CapturedEvent::Motion { x, y, time_ms, .. } => {
                let previous = self.last.replace((*x, *y));
                let (previous_x, previous_y) = previous?;
                let (dx_um, dy_um) = self
                    .monitor
                    .native_delta_to_um(*x - previous_x, *y - previous_y);
                if dx_um == 0 && dy_um == 0 {
                    return None;
                }
                Some(InputEvent::move_relative_um(
                    dx_um,
                    dy_um,
                    u64::from(*time_ms) * 1000,
                ))
            }
            CapturedEvent::Button {
                button,
                pressed,
                time_ms,
                ..
            } => Some(InputEvent::button(
                u16::from(*button),
                *pressed,
                u64::from(*time_ms) * 1000,
            )),
            CapturedEvent::Key {
                keycode,
                pressed,
                time_ms,
                ..
            } => Some(InputEvent::key(
                u16::from(*keycode),
                *pressed,
                u64::from(*time_ms) * 1000,
            )),
        }
    }
}

/// Applies one C1 event through XTest. X11 root coordinates are native pixels,
/// so the physical delta is converted with the native density.
pub fn apply_input(
    injector: &X11Injector,
    monitor: &MonitorInfo,
    event: &InputEvent,
) -> Result<(), InputError> {
    match event.kind {
        InputKind::Move => {
            if event.is_absolute() {
                let max_x = f64::from(monitor.native_w.saturating_sub(1));
                let max_y = f64::from(monitor.native_h.saturating_sub(1));
                let x = round_i16(f64::from(event.x.clamp(0, 65_535)) / 65_535.0 * max_x);
                let y = round_i16(f64::from(event.y.clamp(0, 65_535)) / 65_535.0 * max_y);
                return injector.move_pointer(x, y);
            }
            if !event.is_micrometer_relative() {
                return Err(InputError::Protocol(
                    "relative C1 move must use micrometres".to_owned(),
                ));
            }
            let (dx, dy) = monitor.um_to_native_delta(event.x, event.y);
            injector.move_relative(round_i16(dx), round_i16(dy))
        }
        InputKind::Button => {
            let button = code_to_u8(event.code)?;
            injector.button(button, event.is_down())
        }
        InputKind::Key => {
            let keycode = code_to_u8(event.code)?;
            injector.key(keycode, event.is_down())
        }
        InputKind::Wheel => {
            let (button, ticks) = if event.y != 0 {
                (if event.y < 0 { 4 } else { 5 }, event.y.unsigned_abs())
            } else {
                (if event.x < 0 { 6 } else { 7 }, event.x.unsigned_abs())
            };
            for _ in 0..ticks.min(32) {
                injector.button(button, true)?;
                injector.button(button, false)?;
            }
            Ok(())
        }
    }
}

fn code_to_u8(code: u16) -> Result<u8, InputError> {
    u8::try_from(code).map_err(|_| InputError::Protocol(format!("C1 code {code} does not fit X11")))
}

fn round_i16(value: f64) -> i16 {
    if value > f64::from(i16::MAX) {
        i16::MAX
    } else if value < f64::from(i16::MIN) {
        i16::MIN
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let out = value.round() as i16;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x11::CapturedEvent;
    use cd_geometry::MonitorInfo;

    fn monitor() -> MonitorInfo {
        MonitorInfo::new("linux-4k", 3840, 2160, 1920, 1080, 631.0, 360.0)
    }

    #[test]
    fn motion_becomes_physical_micrometres() {
        let mut mapper = PointerMapper::new(monitor());
        assert!(mapper
            .map(&CapturedEvent::Motion {
                x: 100.0,
                y: 100.0,
                source_id: 2,
                time_ms: 1,
            })
            .is_none());

        let dx = 100.0 * monitor().native_px_per_mm_x();
        let event = mapper
            .map(&CapturedEvent::Motion {
                x: 100.0 + dx,
                y: 100.0,
                source_id: 2,
                time_ms: 2,
            })
            .expect("motion");
        assert_eq!(event.kind, InputKind::Move);
        assert!(event.is_micrometer_relative());
        assert!((event.x - 100_000).abs() <= 1);
        assert_eq!(event.y, 0);
        let (dx_native, _) = monitor().um_to_native_delta(event.x, event.y);
        assert!((dx_native - dx).abs() < 0.5);
    }

    #[test]
    fn duplicate_motion_is_not_emitted() {
        let mut mapper = PointerMapper::new(monitor());
        let event = CapturedEvent::Motion {
            x: 50.0,
            y: 50.0,
            source_id: 2,
            time_ms: 1,
        };
        assert!(mapper.map(&event).is_none());
        assert!(mapper.map(&event).is_none());
    }

    #[test]
    fn buttons_and_keys_map_to_c1() {
        let mut mapper = PointerMapper::new(monitor());
        let button = mapper
            .map(&CapturedEvent::Button {
                button: 1,
                pressed: true,
                source_id: 2,
                time_ms: 10,
            })
            .expect("button");
        assert_eq!(button.kind, InputKind::Button);
        assert_eq!(button.code, 1);
        assert!(button.is_down());

        let key = mapper
            .map(&CapturedEvent::Key {
                keycode: 38,
                pressed: false,
                source_id: 2,
                time_ms: 11,
            })
            .expect("key");
        assert_eq!(key.kind, InputKind::Key);
        assert_eq!(key.code, 38);
        assert!(!key.is_down());
    }

    #[test]
    fn code_conversion_rejects_overflow() {
        assert!(code_to_u8(1).is_ok());
        assert!(code_to_u8(300).is_err());
    }
}
