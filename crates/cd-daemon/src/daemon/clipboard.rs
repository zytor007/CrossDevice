use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::path::PathBuf;
use std::time::Instant;

use cd_clipboard::{ClipboardPolicy, Content, FileRef, LoopGuard, Snapshot};
use cd_geometry::MonitorInfo;
use cd_platform::{FileClipboard, SystemClipboard};
use cd_proto::{EdgeSide, PairSettings};

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const DRAG_EDGE_PX: i32 = 48;
const DRAG_HOVER_MS: u64 = 300;

pub enum Command {
    Apply(Snapshot),
    Stop,
}

pub struct ClipboardHandle {
    commands: mpsc::Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl ClipboardHandle {
    pub fn command_sender(&self) -> mpsc::Sender<Command> {
        self.commands.clone()
    }
}

impl Drop for ClipboardHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Polls the OS clipboard on a dedicated thread (arboard is not async) and
/// publishes only genuinely new local content. Remote writes call `Apply`
/// first, which seeds the last hash so the echo is not uploaded back.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    device_id: String,
    guard: Arc<Mutex<LoopGuard>>,
    outgoing: tokio::sync::mpsc::UnboundedSender<Snapshot>,
    excluded_apps: Vec<String>,
    monitor: MonitorInfo,
    settings: Arc<Mutex<PairSettings>>,
) -> ClipboardHandle {
    let (commands, receiver) = mpsc::channel();
    let join = thread::Builder::new()
        .name("crossdev-clipboard".to_owned())
        .spawn(move || {
            let mut clipboard = match SystemClipboard::new() {
                Ok(clipboard) => clipboard,
                Err(error) => {
                    eprintln!("剪贴板不可用，已禁用 watcher: {error}");
                    return;
                }
            };
            let mut file_clipboard = FileClipboard::open().ok();
            let mut last_text = String::new();
            let mut last_hash = String::new();
            let mut last_image = None::<[u8; 32]>;
            let mut last_files = None::<[u8; 32]>;
            let mut drag_since = None::<Instant>;
            let mut drag_triggered = false;
            let mut suppress_image_until = 0_u64;
            let mut seq = 0_u64;
            let privacy = Privacy::new(excluded_apps);
            loop {
                let mut stop = false;
                while let Ok(command) = receiver.try_recv() {
                    match command {
                        Command::Apply(snapshot) => {
                            last_hash.clone_from(&snapshot.hash);
                            match &snapshot.content {
                                Content::Text { text } => {
                                    last_text.clone_from(text);
                                    last_image = None;
                                    if let Err(error) = clipboard.write_text(text) {
                                        eprintln!("写入系统剪贴板失败: {error}");
                                    }
                                }
                                Content::Image { png, .. } => {
                                    suppress_image_until = now_ms().saturating_add(1_500);
                                    if let Err(error) = clipboard.write_image(png) {
                                        eprintln!("写入系统剪贴板图片失败: {error}");
                                    }
                                }
                                Content::Files { entries } => {
                                    let paths = entries
                                        .iter()
                                        .map(|entry| PathBuf::from(&entry.path))
                                        .collect::<Vec<_>>();
                                    match file_clipboard.as_mut() {
                                        Some(files) => {
                                            if let Err(error) = files.write_paths(&paths) {
                                                eprintln!("写入文件剪贴板失败: {error}");
                                            }
                                        }
                                        None => eprintln!("文件剪贴板不可用，路径未写入"),
                                    }
                                }
                                _ => {}
                            }
                        }
                        Command::Stop => stop = true,
                    }
                }
                if stop {
                    break;
                }

                // While we own a file selection, other apps ask us for the URI
                // list; service those requests promptly and skip text/image
                // polling so the paths are not mirrored as plain text.
                if let Some(files) = file_clipboard.as_mut() {
                    files.service();
                    if files.owns_files() {
                        thread::sleep(Duration::from_millis(40));
                        continue;
                    }
                }

                // File drag hover: a drag that rests near the shared edge for
                // 300 ms becomes a file transfer, without crossing the edge.
                if let Some(files) = file_clipboard.as_mut() {
                    if let Ok((x, _, primary)) = files.pointer() {
                        let pair = settings
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone();
                        let max_x = i32::try_from(monitor.native_w)
                            .unwrap_or(i32::MAX)
                            .saturating_sub(1);
                        let x = i32::from(x);
                        let near_edge = match pair.edge {
                            EdgeSide::Right => x >= max_x - DRAG_EDGE_PX,
                            EdgeSide::Left => x <= DRAG_EDGE_PX,
                        };
                        if primary && near_edge {
                            let now = Instant::now();
                            let since = *drag_since.get_or_insert(now);
                            if now.duration_since(since) < Duration::from_millis(DRAG_HOVER_MS) {
                                thread::sleep(Duration::from_millis(120));
                                continue;
                            }
                            if !drag_triggered && !privacy.is_sensitive() {
                                let paths = files.read_drag_paths().unwrap_or_default();
                                // Some Xdnd owners (Qt/WPS) only serve
                                // destinations that joined the drag protocol;
                                // fall back to the file-list clipboard, which
                                // many file managers populate as well.
                                let paths = if paths.is_empty() {
                                    files.read_clipboard_paths().unwrap_or_default()
                                } else {
                                    paths
                                };
                                if let Some(snapshot) =
                                    files_snapshot(paths, &device_id, seq.saturating_add(1))
                                {
                                    drag_triggered = true;
                                    last_hash.clone_from(&snapshot.hash);
                                    last_text.clear();
                                    last_image = None;
                                    if publish(&guard, &outgoing, &snapshot, &mut seq) {
                                        break;
                                    }
                                }
                            }
                            thread::sleep(Duration::from_millis(120));
                            continue;
                        }
                        if !primary || !near_edge {
                            drag_since = None;
                            drag_triggered = false;
                        }
                    }
                }

                // A file-list selection takes precedence over text/image.
                let mut pending_files = None;
                if let Some(files) = file_clipboard.as_mut() {
                    match files.read_clipboard_paths() {
                        Ok(paths) if !paths.is_empty() => {
                            let signature = paths_signature(&paths);
                            if last_files != Some(signature) {
                                last_files = Some(signature);
                                if !privacy.is_sensitive() {
                                    pending_files =
                                        files_snapshot(paths, &device_id, seq.saturating_add(1));
                                }
                            }
                            match pending_files {
                                Some(snapshot) => {
                                    last_hash.clone_from(&snapshot.hash);
                                    last_text.clear();
                                    last_image = None;
                                    if publish(&guard, &outgoing, &snapshot, &mut seq) {
                                        break;
                                    }
                                }
                                None => {
                                    thread::sleep(Duration::from_millis(350));
                                    continue;
                                }
                            }
                            thread::sleep(Duration::from_millis(350));
                            continue;
                        }
                        Ok(_) => last_files = None,
                        Err(error) => {
                            last_files = None;
                            eprintln!("读取文件剪贴板失败: {error}");
                        }
                    }
                }

                if let Ok(Some(image)) = clipboard.read_image() {
                    let signature = *blake3::hash(&image.rgba).as_bytes();
                    if last_image != Some(signature) {
                        last_image = Some(signature);
                        if now_ms() < suppress_image_until {
                            thread::sleep(Duration::from_millis(350));
                            continue;
                        }
                        if image.rgba.len() as u64 <= MAX_IMAGE_BYTES {
                            if let Ok(png) = image.png() {
                                if png.len() as u64 <= MAX_IMAGE_BYTES {
                                    let snapshot = Snapshot::new(
                                        Content::Image {
                                            png,
                                            width: image.width,
                                            height: image.height,
                                        },
                                        &device_id,
                                        seq.saturating_add(1),
                                    );
                                    if snapshot.hash != last_hash {
                                        last_hash.clone_from(&snapshot.hash);
                                        if !privacy.is_sensitive()
                                            && publish(&guard, &outgoing, &snapshot, &mut seq)
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(350));
                    continue;
                }

                match clipboard.read_text() {
                    Ok(Some(text)) if !text.is_empty() => {
                        if text == last_text {
                            thread::sleep(Duration::from_millis(350));
                            continue;
                        }
                        if text.len() as u64 > cd_clipboard::DEFAULT_AUTO_FETCH_BYTES {
                            eprintln!("剪贴板文本超过自动同步上限，已跳过");
                            last_text = text;
                            thread::sleep(Duration::from_millis(350));
                            continue;
                        }
                        last_image = None;
                        let snapshot = Snapshot::new(
                            Content::text(text.clone()),
                            &device_id,
                            seq.saturating_add(1),
                        );
                        last_text = text;
                        if snapshot.hash != last_hash {
                            last_hash.clone_from(&snapshot.hash);
                            if !privacy.is_sensitive()
                                && publish(&guard, &outgoing, &snapshot, &mut seq)
                            {
                                break;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        // Clipboard managers occasionally lock the selection
                        // for a few milliseconds; retry on the next tick.
                        eprintln!("读取系统剪贴板失败: {error}");
                    }
                }
                thread::sleep(Duration::from_millis(350));
            }
        })
        .ok();
    ClipboardHandle { commands, join }
}

/// Suppresses clipboard publishing while a password manager or another
/// excluded application owns the focused window.
struct Privacy {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    policy: ClipboardPolicy,
    #[cfg(target_os = "linux")]
    window: Option<cd_input::ActiveWindow>,
}

impl Privacy {
    fn new(excluded_apps: Vec<String>) -> Self {
        Self {
            policy: ClipboardPolicy {
                excluded_apps,
                ..ClipboardPolicy::default()
            },
            #[cfg(target_os = "linux")]
            window: cd_input::ActiveWindow::open().ok(),
        }
    }

    fn is_sensitive(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(window) = &self.window {
            if let Some(class) = window.class_name() {
                return self.policy.is_excluded(&class);
            }
        }
        false
    }
}

fn paths_signature(paths: &[PathBuf]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for path in paths {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\0");
    }
    *hasher.finalize().as_bytes()
}

/// Turns a local file selection into a clipboard snapshot, enforcing the
/// 64 MB auto-fetch budget so a stray "copy whole home directory" cannot
/// silently saturate the link.
fn files_snapshot(paths: Vec<PathBuf>, device_id: &str, seq: u64) -> Option<Snapshot> {
    let mut entries = Vec::new();
    let mut total = 0_u64;
    for path in paths.into_iter().take(64) {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        let size = if is_dir {
            match cd_files::scan_dir(&path) {
                Ok(manifest) => manifest.total_bytes(),
                Err(error) => {
                    eprintln!("扫描文件剪贴板目录失败 ({}): {error}", path.display());
                    continue;
                }
            }
        } else {
            metadata.len()
        };
        total = total.saturating_add(size);
        if total > cd_clipboard::DEFAULT_AUTO_FETCH_BYTES {
            eprintln!("文件剪贴板超过 64 MB 自动同步上限，已跳过");
            return None;
        }
        entries.push(FileRef {
            path: path.display().to_string(),
            size,
            is_dir,
        });
    }
    if entries.is_empty() {
        return None;
    }
    Some(Snapshot::new(Content::Files { entries }, device_id, seq))
}

fn publish(
    guard: &Arc<Mutex<LoopGuard>>,
    outgoing: &tokio::sync::mpsc::UnboundedSender<Snapshot>,
    snapshot: &Snapshot,
    seq: &mut u64,
) -> bool {
    let allowed = guard
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .should_publish_local(&snapshot.hash, now_ms());
    if !allowed {
        return false;
    }
    *seq = snapshot.seq;
    outgoing.send(snapshot.clone()).is_err()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
