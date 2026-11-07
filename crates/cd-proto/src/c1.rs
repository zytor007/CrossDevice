use thiserror::Error;

pub const C1_EVENT_LEN: usize = 20;

pub const FLAG_ABSOLUTE: u8 = 1 << 0;
pub const FLAG_DOWN: u8 = 1 << 1;
pub const FLAG_MICROMETER: u8 = 1 << 2;
pub const KNOWN_FLAGS: u8 = FLAG_ABSOLUTE | FLAG_DOWN | FLAG_MICROMETER;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum InputKind {
    Move = 1,
    Button = 2,
    Wheel = 3,
    Key = 4,
}

impl TryFrom<u8> for InputKind {
    type Error = C1DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Move),
            2 => Ok(Self::Button),
            3 => Ok(Self::Wheel),
            4 => Ok(Self::Key),
            other => Err(C1DecodeError::UnknownKind(other)),
        }
    }
}

/// 20-byte C1 input event.
///
/// ```text
/// u8  kind     1=move 2=button 3=wheel 4=key
/// u8  flags    bit0 absolute, bit1 pressed, bit2 relative unit is µm
/// u16 code     HID usage / button / axis
/// i32 x, y     absolute: 0..65535; relative: µm; wheel: platform ticks
/// u64 ts_us    sender monotonic clock, used only locally for ordering
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub flags: u8,
    pub code: u16,
    pub x: i32,
    pub y: i32,
    pub ts_us: u64,
}

impl InputEvent {
    pub fn move_relative_um(dx_um: i32, dy_um: i32, ts_us: u64) -> Self {
        Self {
            kind: InputKind::Move,
            flags: FLAG_MICROMETER,
            code: 0,
            x: dx_um,
            y: dy_um,
            ts_us,
        }
    }

    /// Absolute entry packet. Coordinates are normalised within the shared edge
    /// overlap, not across the whole target screen.
    pub fn move_absolute(x_norm: u16, y_norm: u16, ts_us: u64) -> Self {
        Self {
            kind: InputKind::Move,
            flags: FLAG_ABSOLUTE,
            code: 0,
            x: i32::from(x_norm),
            y: i32::from(y_norm),
            ts_us,
        }
    }

    pub fn button(code: u16, down: bool, ts_us: u64) -> Self {
        Self {
            kind: InputKind::Button,
            flags: if down { FLAG_DOWN } else { 0 },
            code,
            x: 0,
            y: 0,
            ts_us,
        }
    }

    pub fn key(code: u16, down: bool, ts_us: u64) -> Self {
        Self {
            kind: InputKind::Key,
            flags: if down { FLAG_DOWN } else { 0 },
            code,
            x: 0,
            y: 0,
            ts_us,
        }
    }

    pub fn wheel(vertical_ticks: i32, horizontal_ticks: i32, ts_us: u64) -> Self {
        Self {
            kind: InputKind::Wheel,
            flags: 0,
            code: 0,
            x: vertical_ticks,
            y: horizontal_ticks,
            ts_us,
        }
    }

    pub fn is_absolute(&self) -> bool {
        self.flags & FLAG_ABSOLUTE != 0
    }

    pub fn is_down(&self) -> bool {
        self.flags & FLAG_DOWN != 0
    }

    pub fn is_micrometer_relative(&self) -> bool {
        self.flags & FLAG_MICROMETER != 0
    }

    pub fn to_bytes(&self) -> [u8; C1_EVENT_LEN] {
        let mut out = [0_u8; C1_EVENT_LEN];
        out[0] = self.kind as u8;
        out[1] = self.flags;
        out[2..4].copy_from_slice(&self.code.to_le_bytes());
        out[4..8].copy_from_slice(&self.x.to_le_bytes());
        out[8..12].copy_from_slice(&self.y.to_le_bytes());
        out[12..20].copy_from_slice(&self.ts_us.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, C1DecodeError> {
        if bytes.len() != C1_EVENT_LEN {
            return Err(C1DecodeError::WrongLength {
                actual: bytes.len(),
            });
        }
        let kind = InputKind::try_from(bytes[0])?;
        Ok(Self {
            kind,
            flags: bytes[1],
            code: u16::from_le_bytes([bytes[2], bytes[3]]),
            x: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            y: i32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            ts_us: u64::from_le_bytes([
                bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18],
                bytes[19],
            ]),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum C1DecodeError {
    #[error("C1 event must be {C1_EVENT_LEN} bytes, got {actual}")]
    WrongLength { actual: usize },
    #[error("unknown C1 event kind {0}")]
    UnknownKind(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_size_is_exactly_twenty_bytes() {
        let event = InputEvent::move_relative_um(123_456, -78_900, 42);
        assert_eq!(event.to_bytes().len(), 20);
        assert_eq!(event.to_bytes(), event.to_bytes());
    }

    #[test]
    fn roundtrips_every_event_kind() {
        let events = [
            InputEvent::move_relative_um(100_000, -250_000, u64::MAX),
            InputEvent::move_absolute(0, 65_535, 7),
            InputEvent::button(1, true, 9),
            InputEvent::button(1, false, 10),
            InputEvent::key(0x04, true, 11),
            InputEvent::wheel(-3, 0, 12),
        ];
        for event in events {
            assert_eq!(InputEvent::from_bytes(&event.to_bytes()).unwrap(), event);
        }
    }

    #[test]
    fn layout_is_little_endian_and_field_ordered() {
        let event = InputEvent::move_absolute(0x1234, 0x5678, 0x0102_0304_0506_0708);
        let bytes = event.to_bytes();
        assert_eq!(bytes[0], 1);
        assert_eq!(bytes[1], FLAG_ABSOLUTE);
        assert_eq!(&bytes[2..4], &[0, 0]);
        assert_eq!(&bytes[4..8], &0x1234_i32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x5678_i32.to_le_bytes());
        assert_eq!(&bytes[12..20], &0x0102_0304_0506_0708_u64.to_le_bytes());
    }

    #[test]
    fn rejects_wrong_length_and_unknown_kind() {
        assert!(matches!(
            InputEvent::from_bytes(&[0; 19]),
            Err(C1DecodeError::WrongLength { actual: 19 })
        ));
        let mut bytes = InputEvent::move_relative_um(0, 0, 0).to_bytes();
        bytes[0] = 9;
        assert!(matches!(
            InputEvent::from_bytes(&bytes),
            Err(C1DecodeError::UnknownKind(9))
        ));
    }

    #[test]
    fn flags_are_readable() {
        let event = InputEvent::move_relative_um(1, 2, 3);
        assert!(event.is_micrometer_relative());
        assert!(!event.is_absolute());
        assert!(!event.is_down());
    }
}
