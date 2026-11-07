use thiserror::Error;

#[derive(Debug, Error)]
pub enum InputError {
    #[error("cannot connect to the X11 display: {0}")]
    Connect(String),
    #[error("XInput2 is unavailable: {0}")]
    XInput(String),
    #[error("XTest is unavailable: {0}")]
    XTest(String),
    #[error("X11 request failed: {0}")]
    Request(String),
    #[error("input protocol error: {0}")]
    Protocol(String),
    #[error("no X11 display; set DISPLAY or run inside a desktop session")]
    NoDisplay,
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectionError> for InputError {
    fn from(error: x11rb::errors::ConnectionError) -> Self {
        Self::Connect(error.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ReplyError> for InputError {
    fn from(error: x11rb::errors::ReplyError) -> Self {
        Self::Request(error.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ReplyOrIdError> for InputError {
    fn from(error: x11rb::errors::ReplyOrIdError) -> Self {
        Self::Request(error.to_string())
    }
}
