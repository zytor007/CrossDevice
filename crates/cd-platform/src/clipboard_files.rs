//! File-list clipboard support (`text/uri-list`) on X11.
//!
//! The clipboard pipeline transfers files over the C2 data channel, so the
//! receiving daemon needs a way to expose the landed paths as a real OS file
//! clipboard. This module reads file lists from other applications (including
//! the XDND drag selection) and owns the CLIPBOARD selection to serve a
//! `text/uri-list` that file managers can paste.

use std::path::{Path, PathBuf};

use crate::clipboard::ClipboardError;

#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, EventMask};
#[cfg(target_os = "linux")]
use x11rb::protocol::Event;
#[cfg(target_os = "linux")]
use x11rb::rust_connection::RustConnection;
#[cfg(target_os = "linux")]
use x11rb::CURRENT_TIME;

/// Parses a `text/uri-list` body into local paths, ignoring comments,
/// non-file schemes and non-local hosts. Percent escapes are decoded as UTF-8.
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(decode_file_uri)
        .take(64)
        .collect()
}

/// Encodes paths as an X11 `text/uri-list` (CRLF separated, percent escaped).
pub fn encode_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        out.push_str(&file_uri(path));
        out.push_str("\r\n");
    }
    out
}

fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("file://{encoded}")
}

fn decode_file_uri(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("file://")?;
    let index = rest.find('/')?;
    let (host, path) = (&rest[..index], &rest[index..]);
    if !host.is_empty() && host != "localhost" {
        return None;
    }
    Some(PathBuf::from(percent_decode(path)))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
struct X11FileClipboard {
    conn: RustConnection,
    root: u32,
    window: u32,
    property: u32,
    clipboard: u32,
    drag: u32,
    uri_atom: u32,
    targets_atom: u32,
    utf8_atom: u32,
    string_atom: u32,
    text_atom: u32,
    plain_atom: u32,
    stamp_atom: u32,
    owns: bool,
    paths: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl X11FileClipboard {
    fn open() -> Result<Self, ClipboardError> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|error| match error {
            x11rb::errors::ConnectError::DisplayParsingError(_) => {
                ClipboardError::Unavailable("no X11 display".to_owned())
            }
            other => ClipboardError::Unavailable(other.to_string()),
        })?;
        let root = conn.setup().roots[screen_num].root;
        let window = conn
            .generate_id()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE)
                .override_redirect(1),
        )
        .map_err(|error| ClipboardError::Backend(error.to_string()))?
        .check()
        .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        // Some drag sources only inspect XdndAware on mapped windows.
        conn.map_window(window)
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
            .check()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;

        // Drag sources may only serve XdndSelection to destinations that
        // advertise the protocol version on their window.
        let xdnd_aware = intern(&conn, b"XdndAware")?;
        let version: u32 = 5;
        conn.change_property(
            xproto::PropMode::REPLACE,
            window,
            xdnd_aware,
            AtomEnum::ATOM,
            32,
            1,
            &version.to_ne_bytes(),
        )
        .map_err(|error| ClipboardError::Backend(error.to_string()))?
        .check()
        .map_err(|error| ClipboardError::Backend(error.to_string()))?;

        let property = intern(&conn, b"CROSSDEV_SELECTION")?;
        let stamp_atom = intern(&conn, b"CROSSDEV_TIMESTAMP")?;
        let clipboard = intern(&conn, b"CLIPBOARD")?;
        let drag = intern(&conn, b"XdndSelection")?;
        let uri_atom = intern(&conn, b"text/uri-list")?;
        let targets_atom = intern(&conn, b"TARGETS")?;
        let utf8_atom = intern(&conn, b"UTF8_STRING")?;
        let string_atom = u32::from(AtomEnum::STRING);
        let text_atom = intern(&conn, b"text/plain;charset=utf-8")?;
        let plain_atom = intern(&conn, b"text/plain")?;
        Ok(Self {
            conn,
            root,
            window,
            property,
            clipboard,
            drag,
            uri_atom,
            targets_atom,
            utf8_atom,
            string_atom,
            text_atom,
            plain_atom,
            stamp_atom,
            owns: false,
            paths: Vec::new(),
        })
    }

    fn read_clipboard_paths(&mut self) -> Result<Vec<PathBuf>, ClipboardError> {
        self.read_selection(self.clipboard)
    }

    fn read_drag_paths(&mut self) -> Result<Vec<PathBuf>, ClipboardError> {
        self.read_selection(self.drag)
    }

    /// Requests a real server timestamp via a PropertyNotify. Some selection
    /// owners reject conversions made with `CurrentTime`.
    fn timestamp(&mut self) -> Result<u32, ClipboardError> {
        self.conn
            .change_property(
                xproto::PropMode::APPEND,
                self.window,
                self.stamp_atom,
                AtomEnum::STRING,
                8,
                0,
                &[],
            )
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
            .check()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        self.conn
            .flush()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            while let Some(event) = self
                .conn
                .poll_for_event()
                .map_err(|error| ClipboardError::Backend(error.to_string()))?
            {
                match event {
                    Event::PropertyNotify(notify)
                        if notify.window == self.window && notify.atom == self.stamp_atom =>
                    {
                        return Ok(notify.time);
                    }
                    Event::SelectionRequest(request) => self.serve(request)?,
                    Event::SelectionClear(clear) => self.handle_clear(clear),
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Ok(CURRENT_TIME);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn read_selection(&mut self, selection: u32) -> Result<Vec<PathBuf>, ClipboardError> {
        if selection == self.clipboard && self.owns {
            return Ok(self.paths.clone());
        }
        self.service()?;
        let time = self.timestamp()?;
        self.conn
            .convert_selection(self.window, selection, self.uri_atom, self.property, time)
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
            .check()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        self.conn
            .flush()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;

        let deadline = Instant::now() + Duration::from_millis(150);
        loop {
            while let Some(event) = self
                .conn
                .poll_for_event()
                .map_err(|error| ClipboardError::Backend(error.to_string()))?
            {
                match event {
                    Event::SelectionNotify(notify)
                        if notify.requestor == self.window && notify.selection == selection =>
                    {
                        if notify.property == 0 {
                            return Ok(Vec::new());
                        }
                        let reply = self
                            .conn
                            .get_property(
                                true,
                                self.window,
                                notify.property,
                                AtomEnum::ANY,
                                0,
                                8192,
                            )
                            .map_err(|error| ClipboardError::Backend(error.to_string()))?
                            .reply()
                            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
                        let text = String::from_utf8_lossy(&reply.value);
                        return Ok(parse_uri_list(&text));
                    }
                    Event::SelectionRequest(request) => self.serve(request)?,
                    Event::SelectionClear(clear) => self.handle_clear(clear),
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Ok(Vec::new());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn write_paths(&mut self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        self.service()?;
        self.paths = paths.to_vec();
        let time = self.timestamp()?;
        self.conn
            .set_selection_owner(self.window, self.clipboard, time)
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
            .check()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        self.conn
            .flush()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        self.owns = true;
        Ok(())
    }

    fn service(&mut self) -> Result<(), ClipboardError> {
        while let Some(event) = self
            .conn
            .poll_for_event()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
        {
            match event {
                Event::SelectionRequest(request) => self.serve(request)?,
                Event::SelectionClear(clear) => self.handle_clear(clear),
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_clear(&mut self, clear: xproto::SelectionClearEvent) {
        if clear.owner == self.window && clear.selection == self.clipboard {
            self.owns = false;
            self.paths.clear();
        }
    }

    fn serve(&mut self, request: xproto::SelectionRequestEvent) -> Result<(), ClipboardError> {
        if request.owner != self.window || request.selection != self.clipboard {
            return Ok(());
        }
        let property = if request.property == 0 {
            request.target
        } else {
            request.property
        };
        let produced = if request.target == self.targets_atom {
            let atoms = [
                self.targets_atom,
                self.uri_atom,
                self.utf8_atom,
                self.string_atom,
                self.text_atom,
                self.plain_atom,
            ];
            let data = atoms
                .iter()
                .flat_map(|atom| atom.to_ne_bytes())
                .collect::<Vec<u8>>();
            self.conn
                .change_property(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    32,
                    atoms.len() as u32,
                    &data,
                )
                .map_err(|error| ClipboardError::Backend(error.to_string()))?
                .check()
                .map_err(|error| ClipboardError::Backend(error.to_string()))?;
            true
        } else if request.target == self.uri_atom {
            let data = encode_uri_list(&self.paths);
            self.conn
                .change_property(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    self.uri_atom,
                    8,
                    data.len() as u32,
                    data.as_bytes(),
                )
                .map_err(|error| ClipboardError::Backend(error.to_string()))?
                .check()
                .map_err(|error| ClipboardError::Backend(error.to_string()))?;
            true
        } else if request.target == self.utf8_atom
            || request.target == self.string_atom
            || request.target == self.text_atom
            || request.target == self.plain_atom
        {
            let data = self
                .paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            self.conn
                .change_property(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    request.target,
                    8,
                    data.len() as u32,
                    data.as_bytes(),
                )
                .map_err(|error| ClipboardError::Backend(error.to_string()))?
                .check()
                .map_err(|error| ClipboardError::Backend(error.to_string()))?;
            true
        } else {
            false
        };

        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if produced { property } else { 0 },
        };
        self.conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        self.conn
            .flush()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        Ok(())
    }

    fn pointer(&mut self) -> Result<(i16, i16, bool), ClipboardError> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(|error| ClipboardError::Backend(error.to_string()))?
            .reply()
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        let primary = u16::from(reply.mask) & (1 << 8) != 0;
        Ok((reply.root_x, reply.root_y, primary))
    }
}

#[cfg(target_os = "linux")]
fn intern(conn: &RustConnection, name: &[u8]) -> Result<u32, ClipboardError> {
    conn.intern_atom(false, name)
        .map_err(|error| ClipboardError::Backend(error.to_string()))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| ClipboardError::Backend(error.to_string()))
}

/// Cross-platform facade. Only Linux/X11 implements the protocol today; other
/// platforms report it as unavailable so the clipboard pipeline can degrade.
pub struct FileClipboard {
    #[cfg(target_os = "linux")]
    inner: Option<X11FileClipboard>,
}

impl FileClipboard {
    pub fn open() -> Result<Self, ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            X11FileClipboard::open().map(|inner| Self { inner: Some(inner) })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ClipboardError::Unavailable(
                "file clipboard is only implemented on Linux/X11".to_owned(),
            ))
        }
    }

    pub fn is_available(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.inner.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn owns_files(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.inner.as_ref().is_some_and(|inner| inner.owns)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn read_clipboard_paths(&mut self) -> Result<Vec<PathBuf>, ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            match self.inner.as_mut() {
                Some(inner) => inner.read_clipboard_paths(),
                None => Ok(Vec::new()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Vec::new())
        }
    }

    pub fn read_drag_paths(&mut self) -> Result<Vec<PathBuf>, ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            match self.inner.as_mut() {
                Some(inner) => inner.read_drag_paths(),
                None => Ok(Vec::new()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Vec::new())
        }
    }

    pub fn write_paths(&mut self, paths: &[PathBuf]) -> Result<(), ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            match self.inner.as_mut() {
                Some(inner) => inner.write_paths(paths),
                None => Err(ClipboardError::Unavailable(
                    "no X11 file clipboard".to_owned(),
                )),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = paths;
            Err(ClipboardError::Unavailable(
                "file clipboard is only implemented on Linux/X11".to_owned(),
            ))
        }
    }

    pub fn service(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(inner) = self.inner.as_mut() {
            if let Err(error) = inner.service() {
                eprintln!("文件剪贴板服务失败: {error}");
            }
        }
    }

    /// Root pointer position and whether the primary button is held.
    pub fn pointer(&mut self) -> Result<(i16, i16, bool), ClipboardError> {
        #[cfg(target_os = "linux")]
        {
            match self.inner.as_mut() {
                Some(inner) => inner.pointer(),
                None => Ok((0, 0, false)),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok((0, 0, false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_roundtrip_escaped_paths() {
        let paths = vec![
            PathBuf::from("/home/me/report final.pdf"),
            PathBuf::from("/tmp/目录/文件.txt"),
        ];
        let encoded = encode_uri_list(&paths);
        assert!(encoded.contains("report%20final.pdf"));
        assert!(encoded.contains("file:///tmp/"));
        let decoded = parse_uri_list(&encoded);
        assert_eq!(decoded, paths);
    }

    #[test]
    fn uri_list_ignores_comments_and_remote_hosts() {
        let text = "# comment\r\nfile:///tmp/a.txt\r\nfile://remote/b.txt\nhttp://example.com\n";
        let paths = parse_uri_list(text);
        assert_eq!(paths, vec![PathBuf::from("/tmp/a.txt")]);
    }

    #[test]
    fn utf8_percent_escapes_are_decoded() {
        let paths = parse_uri_list("file:///tmp/%E7%9B%AE%E5%BD%95/%E6%96%87%E4%BB%B6.txt\n");
        assert_eq!(paths[0].to_string_lossy(), "/tmp/目录/文件.txt");
    }
}
