#[cfg(target_os = "linux")]
pub mod x11;

mod error;
#[cfg(target_os = "linux")]
mod mapper;

pub use error::InputError;

#[cfg(target_os = "linux")]
pub use mapper::{apply_input, PointerMapper};
#[cfg(target_os = "linux")]
pub use x11::{self_test, ActiveWindow, CapturedEvent, SelfTestReport, X11Capture, X11Injector};
