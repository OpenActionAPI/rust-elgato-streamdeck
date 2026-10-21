//! CORA TCP transport for the Elgato Stream Deck Network Dock.
//!
//! The Network Dock does **not** expose its attached Stream Deck as a USB/IP device. It speaks
//! Elgato's proprietary "CORA" protocol over TCP (default port [`DEFAULT_TCP_PORT`]). The wire
//! format and connection topology mirror the only open implementation, `@elgato-stream-deck/tcp`
//! (github.com/Julusian/node-elgato-stream-deck), and were verified against real dock hardware.
//!
//! Two TCP connections are involved:
//!
//! 1. **Primary** (`host:5343`): the dock itself. After its first keepalive, it answers a
//!    "Device 2" info query (`GET_REPORT 0x1c`) describing the attached deck — vendor/product id,
//!    serial number, and, crucially, the TCP port the deck itself is served on.
//! 2. **Child** (`host:<device2 tcp port>`): the attached deck. Frames sent with the `VERBATIM`
//!    flag tunnel raw HID reports to the deck byte-identically to USB — write, feature reports,
//!    and input reports all flow here — so all per-[`Kind`](crate::info::Kind) encoding in
//!    [`StreamDeck`](crate::StreamDeck) is reused unchanged.
//!
//! Every frame on either connection is a 16-byte header plus payload:
//!
//! ```text
//! frame = 16-byte header + payload
//!   [0..4]   magic  = 43 93 8a 41
//!   [4..6]   flags  u16 LE   (VERBATIM 0x8000 | REQ_ACK 0x4000 | ACK_NAK 0x0200 | RESULT 0x0100)
//!   [6]      hid op u8       (WRITE 0x00 | SEND_REPORT 0x01 | GET_REPORT 0x02)
//!   [7]      unused
//!   [8..12]  message id u32 LE
//!   [12..16] payload length u32 LE
//!   [16..]   payload
//! ```
//!
//! Each connection owns a background reader thread: it answers the dock's keepalive frames
//! (payload `[1, 10, …]` → ACK `[3, 26, connection_no]`), queues input reports, and routes
//! `GET_REPORT` responses back to the blocked caller.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::info::Kind;
use crate::transport::Transport;
use crate::util::extract_str;
use crate::StreamDeckError;

/// Default TCP port the Network Dock listens on.
pub const DEFAULT_TCP_PORT: u16 = 5343;

const MAGIC: [u8; 4] = [0x43, 0x93, 0x8a, 0x41];
const HEADER_LEN: usize = 16;
/// TCP connect / request / keepalive timeout, matching the reference implementation's 5000ms.
const TIMEOUT: Duration = Duration::from_millis(5000);
/// Sanity cap on a frame's advertised payload length, used to resync after corruption.
const MAX_PAYLOAD: usize = 1 << 20;

#[allow(dead_code)] // REQ_ACK / RESULT are documented for completeness; not sent by this client.
mod flags {
    pub const NONE: u16 = 0x0000;
    pub const VERBATIM: u16 = 0x8000;
    pub const REQ_ACK: u16 = 0x4000;
    pub const ACK_NAK: u16 = 0x0200;
    pub const RESULT: u16 = 0x0100;
}

mod op {
    pub const WRITE: u8 = 0x00;
    pub const SEND_REPORT: u8 = 0x01;
    pub const GET_REPORT: u8 = 0x02;
}

/// The attached deck as described by the dock's "Device 2" info report (`GET_REPORT 0x1c`).
#[derive(Debug, Clone)]
pub struct Device2Info {
    /// The deck's USB vendor id.
    pub vendor_id: u16,
    /// The deck's USB product id.
    pub product_id: u16,
    /// The deck's serial number.
    pub serial_number: String,
    /// TCP port on the dock's address where the deck itself is served.
    pub tcp_port: u16,
}

/// Parses a Device 2 info payload (`[0x01, 0x0b, …]`). Returns `Ok(None)` when the report is
/// well-formed but no deck is attached to the dock (status byte != 2).
fn parse_device2_info(payload: &[u8]) -> Result<Option<Device2Info>, StreamDeckError> {
    if payload.len() < 128 {
        return Err(StreamDeckError::Protocol("device2 info response too short"));
    }
    if payload[4] != 0x02 {
        return Ok(None);
    }
    let vendor_id = u16::from_le_bytes([payload[26], payload[27]]);
    let product_id = u16::from_le_bytes([payload[28], payload[29]]);
    // Serial is a nul-padded ascii string in bytes [94..125].
    let serial = &payload[94..125];
    let serial = &serial[..serial.iter().position(|&b| b == 0).unwrap_or(serial.len())];
    let serial_number = extract_str(serial)?;
    let tcp_port = u16::from_le_bytes([payload[126], payload[127]]);
    Ok(Some(Device2Info {
        vendor_id,
        product_id,
        serial_number,
        tcp_port,
    }))
}

/// State shared between a connection's public handle and its background reader thread.
struct Shared {
    /// Write half of the socket. Guarded so request threads and the reader (sending ACKs) serialize.
    writer: Mutex<TcpStream>,
    /// Outgoing message id counter (writes use 0; reads use this, though responses correlate by
    /// report id rather than message id).
    next_id: AtomicU32,
    /// `true` once the first keepalive has been received.
    connected: (Mutex<bool>, Condvar),
    /// Queue of received input reports (full HID report, report id byte included).
    input: (Mutex<VecDeque<Vec<u8>>>, Condvar),
    /// In-flight `GET_REPORT` requests, keyed by report/command id (how responses are matched).
    pending: Mutex<HashMap<u8, SyncSender<Vec<u8>>>>,
    /// Signals the reader thread to stop.
    stop: AtomicBool,
}

/// One CORA TCP connection (to the dock's primary port or to the child deck port) with its
/// background reader thread.
struct Connection {
    shared: Arc<Shared>,
    /// A clone of the socket kept solely to [`shutdown`](TcpStream::shutdown) it on drop, which
    /// unblocks the reader thread's blocking read.
    shutdown_handle: TcpStream,
    reader: Option<JoinHandle<()>>,
}

impl Connection {
    /// Connects to `addr` and waits for the peer's first keepalive. Both the TCP connect and the
    /// keepalive wait are bounded by [`TIMEOUT`].
    fn connect(addr: SocketAddr) -> Result<Self, StreamDeckError> {
        let stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
        stream.set_nodelay(true)?;

        let read_half = stream.try_clone()?;
        let shutdown_handle = stream.try_clone()?;

        let shared = Arc::new(Shared {
            writer: Mutex::new(stream),
            next_id: AtomicU32::new(1),
            connected: (Mutex::new(false), Condvar::new()),
            input: (Mutex::new(VecDeque::new()), Condvar::new()),
            pending: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });

        let reader_shared = Arc::clone(&shared);
        let reader = thread::spawn(move || reader_loop(reader_shared, read_half));

        let connection = Connection {
            shared,
            shutdown_handle,
            reader: Some(reader),
        };

        connection.wait_connected(TIMEOUT)?;
        Ok(connection)
    }

    /// Whether this connection is currently up (keepalive seen, not yet disconnected).
    fn is_connected(&self) -> bool {
        *self.shared.connected.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_connected(&self, timeout: Duration) -> Result<(), StreamDeckError> {
        let (lock, cond) = &self.shared.connected;
        let guard = lock.lock()?;
        let (guard, result) = cond.wait_timeout_while(guard, timeout, |connected| !*connected).map_err(|_| StreamDeckError::PoisonError)?;
        if *guard {
            Ok(())
        } else {
            let _ = result;
            Err(StreamDeckError::Timeout)
        }
    }

    /// Sends a single CORA frame, serializing on the write lock.
    fn send(&self, frame_flags: u16, frame_op: u8, message_id: u32, payload: &[u8]) -> Result<(), StreamDeckError> {
        let frame = encode_frame(frame_flags, frame_op, message_id, payload);
        let mut writer = self.shared.writer.lock()?;
        writer.write_all(&frame)?;
        Ok(())
    }

    /// Issues a `GET_REPORT` with the given flags and request payload, and blocks for the matching
    /// response. `key` is the report/command id the response is correlated by (`payload[1]` on the
    /// primary connection, `payload[0]` on the child; see [`dispatch`]).
    fn get_report(&self, frame_flags: u16, request: &[u8], key: u8) -> Result<Vec<u8>, StreamDeckError> {
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        self.shared.pending.lock()?.insert(key, tx);

        let message_id = self.shared.next_id.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
        if let Err(e) = self.send(frame_flags, op::GET_REPORT, message_id, request) {
            self.shared.pending.lock()?.remove(&key);
            return Err(e);
        }

        match rx.recv_timeout(TIMEOUT) {
            Ok(payload) => Ok(payload),
            Err(_) => {
                self.shared.pending.lock()?.remove(&key);
                if self.is_connected() { Err(StreamDeckError::Timeout) } else { Err(StreamDeckError::Disconnected) }
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Unblock the reader thread's blocking read so it can observe `stop` and exit.
        let _ = self.shutdown_handle.shutdown(Shutdown::Both);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// A [`Transport`] that drives a Stream Deck through a Network Dock over the CORA TCP protocol.
///
/// Holds two connections: the dock's primary port (kept open for keepalives and hotplug state) and
/// the child deck port, which carries all HID traffic.
pub struct CoraTransport {
    /// The dock itself. Unused after discovery except for its keepalive exchange, which the dock
    /// requires to keep the child port alive.
    _primary: Connection,
    /// The attached deck; all reports flow here.
    child: Connection,
    /// Cached Device 2 info from discovery.
    info: Device2Info,
}

impl CoraTransport {
    /// Connects to a Network Dock and then to the deck attached to it.
    ///
    /// Pass the dock's `host:port` address, or use [`DEFAULT_TCP_PORT`]. Discovery queries the
    /// dock's Device 2 info report to find the attached deck's ids and TCP port, then opens the
    /// second connection to the deck itself. Fails with a protocol error when no deck is attached.
    /// (DNS resolution, if the address is not an IP literal, is not bounded by the timeout.)
    pub fn connect<A: ToSocketAddrs>(addr: A) -> Result<Self, StreamDeckError> {
        let mut last_err: Option<StreamDeckError> = None;
        let mut primary = None;
        for sock_addr in addr.to_socket_addrs()? {
            match Connection::connect(sock_addr) {
                Ok(c) => {
                    primary = Some((c, sock_addr));
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let Some((primary, primary_addr)) = primary else {
            return Err(last_err.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "address resolved to no candidates").into()));
        };

        // Device 2 discovery: a primary-port read (flags NONE, request [0x03, id]) whose response
        // is matched by its command id.
        let payload = primary.get_report(flags::NONE, &[0x03, 0x1c], 0x1c)?;
        let info = parse_device2_info(&payload)?.ok_or(StreamDeckError::Protocol("no stream deck attached to network dock"))?;

        let child_addr = SocketAddr::new(primary_addr.ip(), info.tcp_port);
        let child = Connection::connect(child_addr)?;

        Ok(CoraTransport { _primary: primary, child, info })
    }

    /// Whether the deck connection is currently considered up (keepalive seen, not disconnected).
    pub fn is_connected(&self) -> bool {
        self.child.is_connected()
    }

    /// The attached deck's USB vendor and product id, as reported by the dock's Device 2 info.
    pub fn device_vid_pid(&self) -> Result<(u16, u16), StreamDeckError> {
        Ok((self.info.vendor_id, self.info.product_id))
    }

    /// Resolves the attached deck's [`Kind`] from its vendor/product id (see [`device_vid_pid`]).
    ///
    /// [`device_vid_pid`]: CoraTransport::device_vid_pid
    pub fn detect_kind(&self) -> Result<Kind, StreamDeckError> {
        let (vid, pid) = self.device_vid_pid()?;
        Kind::from_vid_pid(vid, pid).ok_or(StreamDeckError::UnrecognizedPID)
    }
}

impl Transport for CoraTransport {
    fn read_report(&self, length: usize, timeout: Option<Duration>) -> Result<Vec<u8>, StreamDeckError> {
        let (lock, cond) = &self.child.shared.input;
        let mut queue = lock.lock()?;

        if queue.is_empty() {
            if !self.child.is_connected() {
                return Err(StreamDeckError::Disconnected);
            }
            match timeout {
                // Mirror HID `read` with blocking disabled: return "no data" immediately.
                None => return Ok(vec![0u8; length]),
                Some(timeout) => {
                    // The reader thread notifies this condvar on disconnect too, so the wait ends
                    // as soon as the link drops rather than running out the full timeout.
                    let (guard, _) = cond
                        .wait_timeout_while(queue, timeout, |q| q.is_empty() && self.child.is_connected())
                        .map_err(|_| StreamDeckError::PoisonError)?;
                    queue = guard;
                    if queue.is_empty() {
                        if !self.child.is_connected() {
                            return Err(StreamDeckError::Disconnected);
                        }
                        return Ok(vec![0u8; length]);
                    }
                }
            }
        }

        let mut payload = queue.pop_front().unwrap_or_default();
        drop(queue);
        // Match HID `read`, which fills a fixed-size buffer: truncate or zero-pad to `length` so
        // the per-`Kind` parsers in `lib.rs` see exactly the bytes they expect.
        payload.resize(length, 0);
        Ok(payload)
    }

    fn write_report(&self, payload: &[u8]) -> Result<usize, StreamDeckError> {
        self.child.send(flags::VERBATIM, op::WRITE, 0, payload)?;
        Ok(payload.len())
    }

    fn get_feature_report(&self, report_id: u8, length: usize) -> Result<Vec<u8>, StreamDeckError> {
        // A VERBATIM read on the child connection is a raw hid_get_feature_report on the deck: the
        // request is just the report id, and the response payload arrives id-prefixed like HID.
        let mut data = self.child.get_report(flags::VERBATIM, &[report_id], report_id)?;
        // Match `HidDevice::get_feature_report`, which fills a `length + 1` buffer.
        data.resize(length + 1, 0);
        Ok(data)
    }

    fn send_feature_report(&self, payload: &[u8]) -> Result<(), StreamDeckError> {
        self.child.send(flags::VERBATIM, op::SEND_REPORT, 0, payload)
    }

    fn manufacturer(&self) -> Result<String, StreamDeckError> {
        Ok("Elgato".to_string())
    }

    fn product(&self) -> Result<String, StreamDeckError> {
        Ok("Stream Deck (Network Dock)".to_string())
    }

    fn serial_number(&self, _kind: Kind) -> Result<String, StreamDeckError> {
        // Already known from Device 2 discovery; avoids a round trip.
        Ok(self.info.serial_number.clone())
    }

    // firmware_version: the trait's default USB feature-report implementation works verbatim
    // through the child connection.
}

/// Encodes a CORA frame: 16-byte header followed by `payload`.
fn encode_frame(frame_flags: u16, frame_op: u8, message_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&frame_flags.to_le_bytes());
    buf.push(frame_op);
    buf.push(0); // byte 7 is unused
    buf.extend_from_slice(&message_id.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Background reader: pulls bytes off the socket, reframes CORA packets, and dispatches them.
fn reader_loop(shared: Arc<Shared>, mut stream: TcpStream) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break, // peer closed
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                process_buffer(&shared, &mut buf);
            }
            Err(_) => break,
        }
    }

    // Connection is gone: wake any waiters and fail outstanding requests.
    {
        let (lock, cond) = &shared.connected;
        if let Ok(mut connected) = lock.lock() {
            *connected = false;
        }
        cond.notify_all();
    }
    if let Ok(mut pending) = shared.pending.lock() {
        pending.clear();
    }
    shared.input.1.notify_all();
}

/// Consumes whole frames from `buf`, leaving any partial trailing frame in place.
fn process_buffer(shared: &Arc<Shared>, buf: &mut Vec<u8>) {
    loop {
        // Resync to the magic bytes if necessary.
        if buf.len() < MAGIC.len() {
            return;
        }
        if buf[..MAGIC.len()] != MAGIC {
            match find_subslice(buf, &MAGIC) {
                Some(index) => {
                    buf.drain(..index);
                }
                None => {
                    // Keep the last few bytes in case the magic is split across reads.
                    let keep = buf.len().min(MAGIC.len() - 1);
                    let start = buf.len() - keep;
                    buf.drain(..start);
                    return;
                }
            }
        }

        if buf.len() < HEADER_LEN {
            return;
        }

        let payload_len = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
        if payload_len > MAX_PAYLOAD {
            // Almost certainly desync/corruption: skip this magic and search for the next.
            buf.drain(..MAGIC.len());
            continue;
        }
        if buf.len() < HEADER_LEN + payload_len {
            return; // wait for the rest of the payload
        }

        let frame_flags = u16::from_le_bytes([buf[4], buf[5]]);
        let frame_op = buf[6];
        let message_id = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let payload = buf[HEADER_LEN..HEADER_LEN + payload_len].to_vec();
        buf.drain(..HEADER_LEN + payload_len);

        dispatch(shared, frame_flags, frame_op, message_id, payload);
    }
}

/// Routes a parsed frame: keepalive ACK, Device 2 info, input report, or `GET_REPORT` response.
fn dispatch(shared: &Arc<Shared>, frame_flags: u16, frame_op: u8, message_id: u32, payload: Vec<u8>) {
    // Keepalive: payload starts with [1, 10, …]. Reply with an ACK and mark the link up.
    if payload.len() > 4 && payload[0] == 1 && payload[1] == 10 {
        {
            let (lock, cond) = &shared.connected;
            if let Ok(mut connected) = lock.lock()
                && !*connected
            {
                *connected = true;
                cond.notify_all();
            }
        }

        let connection_no = payload.get(5).copied().unwrap_or(0);
        let mut ack = vec![0u8; 32];
        ack[0] = 3;
        ack[1] = 26;
        ack[2] = connection_no;

        let frame = encode_frame(flags::ACK_NAK, frame_op, message_id, &ack);
        if let Ok(mut writer) = shared.writer.lock() {
            let _ = writer.write_all(&frame);
        }
        return;
    }

    // Device 2 info ([0x01, 0x0b, …], primary connection only): a response to the 0x1c query, or
    // an unsolicited hotplug event (which no pending request matches, and is dropped).
    if payload.len() > 1 && payload[0] == 0x01 && payload[1] == 0x0b {
        resolve_pending(shared, 0x1c, payload);
        return;
    }

    // An input report (child connection): payload[0] == 0x01 is the HID report id, kept so
    // `lib.rs` parsers (which expect the id-prefixed report) work unchanged.
    if !payload.is_empty() && payload[0] == 0x01 {
        let (lock, cond) = &shared.input;
        if let Ok(mut queue) = lock.lock() {
            queue.push_back(payload);
            cond.notify_one();
        }
        return;
    }

    // Otherwise it is a GET_REPORT response. VERBATIM (child) responses key on payload[0]; others
    // (primary port) key on payload[1].
    let key = if frame_flags & flags::VERBATIM != 0 { payload.first().copied() } else { payload.get(1).copied() };
    if let Some(key) = key {
        resolve_pending(shared, key, payload);
    }
}

/// Hands `payload` to a waiting [`Connection::get_report`] call, if one is pending for `key`.
fn resolve_pending(shared: &Arc<Shared>, key: u8, payload: Vec<u8>) {
    let sender = shared.pending.lock().ok().and_then(|mut p| p.remove(&key));
    if let Some(sender) = sender {
        let _ = sender.try_send(payload);
    }
}

/// Returns the index of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn keepalive_frame(connection_no: u8) -> Vec<u8> {
        encode_frame(flags::NONE, 0x00, 0, &[1, 10, 0, 0, 0, connection_no])
    }

    /// Builds a 512-byte Device 2 info payload like a real dock's (field layout captured from
    /// hardware: an Elgato Network Dock with a Stream Deck Plus attached).
    fn device2_payload(connected: bool, vid: u16, pid: u16, serial: &[u8], tcp_port: u16) -> Vec<u8> {
        let mut p = vec![0u8; 512];
        p[0] = 0x01;
        p[1] = 0x0b;
        p[4] = if connected { 0x02 } else { 0x00 };
        p[26..28].copy_from_slice(&vid.to_le_bytes());
        p[28..30].copy_from_slice(&pid.to_le_bytes());
        p[94..94 + serial.len()].copy_from_slice(serial);
        p[126..128].copy_from_slice(&tcp_port.to_le_bytes());
        p
    }

    /// Reads one whole CORA frame from a socket, returning `(flags, op, message_id, payload)`.
    fn read_frame(sock: &mut std::net::TcpStream) -> (u16, u8, u32, Vec<u8>) {
        let mut header = [0u8; HEADER_LEN];
        sock.read_exact(&mut header).unwrap();
        assert_eq!(&header[0..4], &MAGIC);
        let frame_flags = u16::from_le_bytes([header[4], header[5]]);
        let frame_op = header[6];
        let message_id = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let payload_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
        let mut payload = vec![0u8; payload_len];
        sock.read_exact(&mut payload).unwrap();
        (frame_flags, frame_op, message_id, payload)
    }

    /// Serves the primary-port side of discovery: keepalive, ACK, then the Device 2 query.
    /// Returns the accepted socket so the caller can keep the connection open.
    fn serve_primary_discovery(listener: &TcpListener, device2: &[u8]) -> std::net::TcpStream {
        let (mut sock, _) = listener.accept().unwrap();

        sock.write_all(&keepalive_frame(1)).unwrap();
        let mut ack = [0u8; HEADER_LEN + 32];
        sock.read_exact(&mut ack).unwrap();
        assert_eq!(&ack[0..4], &MAGIC);
        assert_eq!(&ack[4..6], &flags::ACK_NAK.to_le_bytes());
        assert_eq!(&ack[16..19], &[3, 26, 1]); // [3, 26, connection_no]

        let (frame_flags, frame_op, _, request) = read_frame(&mut sock);
        assert_eq!(frame_flags, flags::NONE);
        assert_eq!(frame_op, op::GET_REPORT);
        assert_eq!(request, [0x03, 0x1c]);
        sock.write_all(&encode_frame(flags::NONE, op::GET_REPORT, 0, device2)).unwrap();

        sock
    }

    #[test]
    fn encode_frame_has_exact_layout() {
        let frame = encode_frame(flags::VERBATIM, op::WRITE, 0x0102_0304, &[0xAA, 0xBB]);
        assert_eq!(&frame[0..4], &MAGIC);
        assert_eq!(&frame[4..6], &0x8000u16.to_le_bytes()); // VERBATIM, LE
        assert_eq!(frame[6], 0x00); // WRITE
        assert_eq!(frame[7], 0x00); // unused
        assert_eq!(&frame[8..12], &0x0102_0304u32.to_le_bytes()); // message id, LE
        assert_eq!(&frame[12..16], &2u32.to_le_bytes()); // payload length, LE
        assert_eq!(&frame[16..18], &[0xAA, 0xBB]);
    }

    #[test]
    fn find_subslice_locates_magic() {
        assert_eq!(find_subslice(&[0, 1, 0x43, 0x93, 0x8a, 0x41, 9], &MAGIC), Some(2));
        assert_eq!(find_subslice(&[0, 1, 2, 3], &MAGIC), None);
    }

    /// Field offsets verified against a real dock's response (Stream Deck Plus attached).
    #[test]
    fn parse_device2_info_extracts_fields() {
        let payload = device2_payload(true, 0x0fd9, 0x0084, b"A00WA5321KAGU2", 20001);
        let info = parse_device2_info(&payload).unwrap().unwrap();
        assert_eq!(info.vendor_id, 0x0fd9);
        assert_eq!(info.product_id, 0x0084);
        assert_eq!(info.serial_number, "A00WA5321KAGU2");
        assert_eq!(info.tcp_port, 20001);
        assert_eq!(Kind::from_vid_pid(info.vendor_id, info.product_id), Some(Kind::Plus));
    }

    #[test]
    fn parse_device2_info_no_deck_attached() {
        let payload = device2_payload(false, 0, 0, b"", 0);
        assert!(parse_device2_info(&payload).unwrap().is_none());
        assert!(parse_device2_info(&[0u8; 16]).is_err());
    }

    /// Stands up a loopback "dock" (primary + child listeners): discovery finds the child port,
    /// input reports arrive on the child connection (split across writes to exercise reframing),
    /// and writes go to the child with the VERBATIM flag.
    #[test]
    fn discovery_and_child_input_roundtrip() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();
        let child_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let child_port = child_listener.local_addr().unwrap().port();

        let device2 = device2_payload(true, 0x0fd9, 0x0084, b"A00WA5321KAGU2", child_port);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(500));
        });

        let child = thread::spawn(move || {
            let (mut sock, _) = child_listener.accept().unwrap();

            sock.write_all(&keepalive_frame(7)).unwrap();
            let mut ack = [0u8; HEADER_LEN + 32];
            sock.read_exact(&mut ack).unwrap();
            assert_eq!(&ack[16..19], &[3, 26, 7]);

            // An input report, deliberately split mid-magic to exercise reframing.
            let input = encode_frame(flags::NONE, 0x00, 0, &[0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB]);
            sock.write_all(&input[..3]).unwrap();
            thread::sleep(Duration::from_millis(50));
            sock.write_all(&input[3..]).unwrap();

            // A write from the transport must arrive VERBATIM on the child connection.
            let (frame_flags, frame_op, _, payload) = read_frame(&mut sock);
            assert_eq!(frame_flags, flags::VERBATIM);
            assert_eq!(frame_op, op::WRITE);
            assert_eq!(payload, [0x02, 0x0c]);

            thread::sleep(Duration::from_millis(300));
        });

        let transport = CoraTransport::connect(primary_addr).unwrap();
        assert!(transport.is_connected());
        assert_eq!(transport.device_vid_pid().unwrap(), (0x0fd9, 0x0084));
        assert_eq!(transport.detect_kind().unwrap(), Kind::Plus);

        let data = transport.read_report(6, Some(Duration::from_secs(2))).unwrap();
        assert_eq!(data, vec![0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB]);

        // No input pending: a `None` timeout must return immediately as "no data".
        let empty = transport.read_report(6, None).unwrap();
        assert_eq!(empty, vec![0u8; 6]);

        transport.write_report(&[0x02, 0x0c]).unwrap();

        drop(transport);
        primary.join().unwrap();
        child.join().unwrap();
    }

    /// Serial comes from Device 2 info (no wire call); feature reports tunnel VERBATIM to the
    /// child, where the trait's default per-Kind decoding applies (firmware for a Plus: report
    /// 0x05, ascii at bytes [6..]).
    #[test]
    fn serial_from_device2_and_firmware_over_child() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();
        let child_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let child_port = child_listener.local_addr().unwrap().port();

        let device2 = device2_payload(true, 0x0fd9, 0x0084, b"AB12CD34", child_port);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(500));
        });

        let child = thread::spawn(move || {
            let (mut sock, _) = child_listener.accept().unwrap();

            sock.write_all(&keepalive_frame(1)).unwrap();
            let mut ack = [0u8; HEADER_LEN + 32];
            sock.read_exact(&mut ack).unwrap();

            let (frame_flags, frame_op, _, request) = read_frame(&mut sock);
            assert_eq!(frame_flags, flags::VERBATIM);
            assert_eq!(frame_op, op::GET_REPORT);
            assert_eq!(request, [0x05]);

            let mut response = vec![0u8; 32];
            response[0] = 0x05;
            response[6..14].copy_from_slice(b"1.00.001");
            sock.write_all(&encode_frame(flags::VERBATIM, op::GET_REPORT, 0, &response)).unwrap();

            thread::sleep(Duration::from_millis(100));
        });

        let transport = CoraTransport::connect(primary_addr).unwrap();
        assert_eq!(transport.serial_number(Kind::Plus).unwrap(), "AB12CD34");
        assert_eq!(transport.firmware_version(Kind::Plus).unwrap(), "1.00.001");

        drop(transport);
        primary.join().unwrap();
        child.join().unwrap();
    }

    /// The deck dropping the child connection mid-session must surface as `Disconnected` —
    /// promptly, not after the caller's full read timeout.
    #[test]
    fn read_report_errors_after_disconnect() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();
        let child_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let child_port = child_listener.local_addr().unwrap().port();

        let device2 = device2_payload(true, 0x0fd9, 0x0084, b"AB12CD34", child_port);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(500));
        });

        let child = thread::spawn(move || {
            let (mut sock, _) = child_listener.accept().unwrap();
            sock.write_all(&keepalive_frame(1)).unwrap();
            let mut ack = [0u8; HEADER_LEN + 32];
            sock.read_exact(&mut ack).unwrap();
            thread::sleep(Duration::from_millis(100));
            // Socket drops here: the deck went away.
        });

        let transport = CoraTransport::connect(primary_addr).unwrap();
        child.join().unwrap();

        let start = std::time::Instant::now();
        let result = transport.read_report(6, Some(Duration::from_secs(10)));
        assert!(matches!(result, Err(StreamDeckError::Disconnected)), "got {result:?}");
        assert!(start.elapsed() < Duration::from_secs(2), "read did not end promptly on disconnect: {:?}", start.elapsed());
        assert!(!transport.is_connected());
        assert!(matches!(transport.read_report(6, None), Err(StreamDeckError::Disconnected)));

        primary.join().unwrap();
    }

    /// Several frames arriving in a single TCP segment (keepalive + two input reports) must all be
    /// dispatched — real docks batch writes like this.
    #[test]
    fn coalesced_frames_in_one_write() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();
        let child_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let child_port = child_listener.local_addr().unwrap().port();

        let device2 = device2_payload(true, 0x0fd9, 0x0084, b"AB12CD34", child_port);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(500));
        });

        let child = thread::spawn(move || {
            let (mut sock, _) = child_listener.accept().unwrap();

            // Keepalive and both input reports coalesced into one write.
            let mut batch = keepalive_frame(1);
            batch.extend_from_slice(&encode_frame(flags::NONE, 0x00, 0, &[0x01, 0x00, 0x00, 0x00, 0xAA]));
            batch.extend_from_slice(&encode_frame(flags::NONE, 0x00, 0, &[0x01, 0x00, 0x00, 0x00, 0xBB]));
            sock.write_all(&batch).unwrap();

            let mut ack = [0u8; HEADER_LEN + 32];
            sock.read_exact(&mut ack).unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let transport = CoraTransport::connect(primary_addr).unwrap();
        let timeout = Some(Duration::from_secs(2));
        assert_eq!(transport.read_report(5, timeout).unwrap(), vec![0x01, 0x00, 0x00, 0x00, 0xAA]);
        assert_eq!(transport.read_report(5, timeout).unwrap(), vec![0x01, 0x00, 0x00, 0x00, 0xBB]);

        drop(transport);
        primary.join().unwrap();
        child.join().unwrap();
    }

    /// Keepalives keep coming for the lifetime of a session; each one must be ACKed, and the
    /// connection must stay usable throughout.
    #[test]
    fn ongoing_keepalives_are_acked() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();
        let child_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let child_port = child_listener.local_addr().unwrap().port();

        let device2 = device2_payload(true, 0x0fd9, 0x0084, b"AB12CD34", child_port);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(500));
        });

        let child = thread::spawn(move || {
            let (mut sock, _) = child_listener.accept().unwrap();

            for round in 0..3u8 {
                sock.write_all(&keepalive_frame(7 + round)).unwrap();
                let mut ack = [0u8; HEADER_LEN + 32];
                sock.read_exact(&mut ack).unwrap();
                assert_eq!(&ack[0..4], &MAGIC);
                assert_eq!(&ack[4..6], &flags::ACK_NAK.to_le_bytes());
                // The ACK must echo each keepalive's connection number.
                assert_eq!(&ack[16..19], &[3, 26, 7 + round]);
            }

            // Still usable after several keepalive rounds.
            sock.write_all(&encode_frame(flags::NONE, 0x00, 0, &[0x01, 0x00, 0x00, 0x00, 0xCC])).unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        let transport = CoraTransport::connect(primary_addr).unwrap();
        let data = transport.read_report(5, Some(Duration::from_secs(2))).unwrap();
        assert_eq!(data, vec![0x01, 0x00, 0x00, 0x00, 0xCC]);
        assert!(transport.is_connected());

        drop(transport);
        primary.join().unwrap();
        child.join().unwrap();
    }

    /// A dock with no deck attached must fail discovery rather than pretending to connect.
    #[test]
    fn connect_fails_when_no_deck_attached() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let primary_addr = primary_listener.local_addr().unwrap();

        let device2 = device2_payload(false, 0, 0, b"", 0);
        let primary = thread::spawn(move || {
            let _sock = serve_primary_discovery(&primary_listener, &device2);
            thread::sleep(Duration::from_millis(200));
        });

        let result = CoraTransport::connect(primary_addr);
        assert!(matches!(result, Err(StreamDeckError::Protocol(_))));

        primary.join().unwrap();
    }

    /// Connecting to an unreachable (non-routable) address must fail within roughly `TIMEOUT`
    /// rather than hanging for the kernel's TCP connect timeout. Ignored by default: depends on
    /// the network black-holing 10.255.255.1, which some CI environments don't.
    #[test]
    #[ignore]
    fn connect_to_unreachable_address_times_out() {
        let start = std::time::Instant::now();
        let result = CoraTransport::connect("10.255.255.1:5343");
        assert!(result.is_err());
        assert!(start.elapsed() < TIMEOUT + Duration::from_secs(2), "connect took {:?}, expected ~{TIMEOUT:?}", start.elapsed());
    }
}
