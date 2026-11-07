//! Non-Linux placeholder so the daemon keeps clipboard/file/HTTP services on
//! macOS and Windows. Native capture/injection backends plug in here.

use std::sync::{Arc, Mutex};

use cd_core::OwnershipMachine;
use cd_geometry::MonitorInfo;
use cd_proto::{InputEvent, PairSettings};

use super::registry::PeerRegistry;
use super::status::StatusBoard;

pub struct InputHandle;

impl InputHandle {
    pub fn inject(&self, _event: InputEvent) {}
    pub fn release(&self) {}
}

pub fn spawn(
    _local_id: String,
    _monitor: MonitorInfo,
    _registry: PeerRegistry,
    _ownership: Arc<Mutex<OwnershipMachine>>,
    _status: Arc<StatusBoard>,
    _settings: Arc<Mutex<PairSettings>>,
) -> Option<InputHandle> {
    eprintln!("当前平台尚未实现输入泵；剪贴板、文件与浏览器控制台仍可用");
    None
}
