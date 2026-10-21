//! Transport abstraction for Stream Deck devices.
//!
//! [`StreamDeck`](crate::StreamDeck) talks to hardware through a [`Transport`]. The default
//! [`HidTransport`] speaks USB HID via [`hidapi`]; alternative transports (such as the Network
//! Dock's CORA TCP protocol) can be plugged in without touching any per-[`Kind`](crate::info::Kind)
//! encoding — all command/image encoding lives in [`StreamDeck`] and bottoms out in this trait.

use std::time::Duration;

use hidapi::HidDevice;

use crate::info::Kind;
use crate::util::{extract_str, get_feature_report, read_data, send_feature_report, write_data};
use crate::StreamDeckError;

/// CORA TCP transport for the Network Dock
#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub mod cora;
#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub use cora::{CoraTransport, DEFAULT_TCP_PORT};

/// A raw byte sink/source for Stream Deck HID reports.
///
/// A transport only moves raw report bytes to and from the device; it performs no per-device
/// encoding. Implement this to add a new way of reaching a Stream Deck (e.g. over TCP).
///
/// `Send` is required so that [`StreamDeck`](crate::StreamDeck) (and the async wrapper that holds
/// it behind a mutex) can be moved across threads, matching the HID transport's behaviour.
pub trait Transport: Send {
    /// Reads a single input report of `length` bytes. Blocks up to `timeout`, or indefinitely if
    /// `None` (matching the HID semantics the async wrapper relies on for its poll loop).
    fn read_report(&self, length: usize, timeout: Option<Duration>) -> Result<Vec<u8>, StreamDeckError>;

    /// Writes an output report (HID interrupt OUT, or a CORA `WRITE`).
    fn write_report(&self, payload: &[u8]) -> Result<usize, StreamDeckError>;

    /// Reads a feature report with the given report id. Returns the report bytes with the id byte
    /// prepended, i.e. `length + 1` bytes, matching [`HidDevice::get_feature_report`].
    fn get_feature_report(&self, report_id: u8, length: usize) -> Result<Vec<u8>, StreamDeckError>;

    /// Sends a feature report. `payload[0]` is the report id.
    fn send_feature_report(&self, payload: &[u8]) -> Result<(), StreamDeckError>;

    /// Device manufacturer string.
    fn manufacturer(&self) -> Result<String, StreamDeckError>;

    /// Device product string.
    fn product(&self) -> Result<String, StreamDeckError>;

    /// Reads the device's serial number.
    ///
    /// The default implementation reads it via the USB feature reports, with the report id and
    /// string offset selected per [`Kind`]. Transports whose identity protocol differs (such as
    /// CORA) override this.
    fn serial_number(&self, kind: Kind) -> Result<String, StreamDeckError> {
        match kind {
            Kind::Original | Kind::Mini => {
                let bytes = self.get_feature_report(0x03, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            Kind::MiniMk2 | Kind::MiniDiscord | Kind::MiniMk2Module => {
                let bytes = self.get_feature_report(0x03, 32)?;
                Ok(extract_str(&bytes[5..])?)
            }

            _ => {
                let bytes = self.get_feature_report(0x06, 32)?;
                Ok(extract_str(&bytes[2..])?)
            }
        }
        .map(|s| s.replace('\u{0001}', ""))
    }

    /// Reads the device's firmware version.
    ///
    /// The default implementation reads it via the USB feature reports, with the report id and
    /// string offset selected per [`Kind`]. Transports whose identity protocol differs (such as
    /// CORA) override this.
    fn firmware_version(&self, kind: Kind) -> Result<String, StreamDeckError> {
        match kind {
            Kind::Original | Kind::Mini | Kind::MiniMk2 | Kind::MiniDiscord => {
                let bytes = self.get_feature_report(0x04, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            Kind::MiniMk2Module => {
                let bytes = self.get_feature_report(0xA1, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            _ => {
                let bytes = self.get_feature_report(0x05, 32)?;
                Ok(extract_str(&bytes[6..])?)
            }
        }
    }
}

/// USB HID transport backed by [`hidapi`]. This is the transport used by [`StreamDeck::connect`].
///
/// [`StreamDeck::connect`]: crate::StreamDeck::connect
pub struct HidTransport {
    device: HidDevice,
}

impl HidTransport {
    /// Wraps an already-open [`HidDevice`].
    pub fn new(device: HidDevice) -> Self {
        Self { device }
    }
}

impl Transport for HidTransport {
    fn read_report(&self, length: usize, timeout: Option<Duration>) -> Result<Vec<u8>, StreamDeckError> {
        Ok(read_data(&self.device, length, timeout)?)
    }

    fn write_report(&self, payload: &[u8]) -> Result<usize, StreamDeckError> {
        Ok(write_data(&self.device, payload)?)
    }

    fn get_feature_report(&self, report_id: u8, length: usize) -> Result<Vec<u8>, StreamDeckError> {
        Ok(get_feature_report(&self.device, report_id, length)?)
    }

    fn send_feature_report(&self, payload: &[u8]) -> Result<(), StreamDeckError> {
        Ok(send_feature_report(&self.device, payload)?)
    }

    fn manufacturer(&self) -> Result<String, StreamDeckError> {
        Ok(self.device.get_manufacturer_string()?.unwrap_or_else(|| "Unknown".to_string()))
    }

    fn product(&self) -> Result<String, StreamDeckError> {
        Ok(self.device.get_product_string()?.unwrap_or_else(|| "Unknown".to_string()))
    }
}
