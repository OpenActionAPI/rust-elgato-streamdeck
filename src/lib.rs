//! Elgato Streamdeck library
//!
//! Library for interacting with Elgato Stream Decks through [hidapi](https://crates.io/crates/hidapi).
//! Heavily based on [python-elgato-streamdeck](https://github.com/abcminiuser/python-elgato-streamdeck) and partially on
//! [streamdeck library for rust](https://github.com/ryankurte/rust-streamdeck).

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::iter::zip;
use std::str::Utf8Error;
use std::sync::RwLock;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::images::{convert_image, ImageRect};
use hidapi::{HidApi, HidDevice, HidError, HidResult};
use image::{DynamicImage, ImageError};

use crate::info::{is_vendor_familiar, Kind};
use crate::util::{extract_str, flip_key_index, get_feature_report, read_button_states, read_data, read_encoder_input, read_lcd_input, send_feature_report, write_data};

/// Various information about Stream Deck devices
pub mod info;
/// Utility functions for working with Stream Deck devices
pub mod util;
/// Image processing functions
pub mod images;

/// Async Stream Deck
#[cfg(feature = "async")]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub mod asynchronous;
#[cfg(feature = "async")]
#[cfg_attr(docsrs, doc(cfg(feature = "async")))]
pub use asynchronous::AsyncStreamDeck;

/// Creates an instance of the HidApi
///
/// Can be used if you don't want to link hidapi crate into your project
pub fn new_hidapi() -> HidResult<HidApi> {
    HidApi::new()
}

/// Actually refreshes the device list
pub fn refresh_device_list(hidapi: &mut HidApi) -> HidResult<()> {
    hidapi.refresh_devices()
}

/// Returns a list of devices as (Kind, Serial Number) that could be found using HidApi.
///
/// **WARNING:** To refresh the list, use [refresh_device_list]
pub fn list_devices(hidapi: &HidApi) -> Vec<(Kind, String)> {
    hidapi
        .device_list()
        .filter_map(|d| {
            if !is_vendor_familiar(&d.vendor_id()) {
                return None;
            }

            if let Some(serial) = d.serial_number() {
                Some((Kind::from_vid_pid(d.vendor_id(), d.product_id())?, serial.to_string()))
            } else {
                None
            }
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// Type of input that the device produced
#[derive(Clone, Debug)]
pub enum StreamDeckInput {
    /// No data was passed from the device
    NoData,

    /// Button was pressed
    ButtonStateChange(Vec<bool>),

    /// Encoder/Knob was pressed
    EncoderStateChange(Vec<bool>),

    /// Encoder/Knob was twisted/turned
    EncoderTwist(Vec<i8>),

    /// Touch screen received short press
    TouchScreenPress(u16, u16),

    /// Touch screen received long press
    TouchScreenLongPress(u16, u16),

    /// Touch screen received a swipe
    TouchScreenSwipe((u16, u16), (u16, u16)),
}

impl StreamDeckInput {
    /// Checks if there's data received or not
    pub fn is_empty(&self) -> bool {
        matches!(self, StreamDeckInput::NoData)
    }
}

/// Interface for a Stream Deck device
pub struct StreamDeck {
    /// Kind of the device
    kind: Kind,
    /// Connected HIDDevice
    device: HidDevice,
    /// Temporarily cache the image before sending it to the device
    image_cache: RwLock<Vec<ImageCache>>,
}

struct ImageCache {
    key: u8,
    image_data: Vec<u8>,
}

/// Static functions of the struct
impl StreamDeck {
    /// Attempts to connect to the device
    pub fn connect(hidapi: &HidApi, kind: Kind, serial: &str) -> Result<StreamDeck, StreamDeckError> {
        let device = hidapi.open_serial(kind.vendor_id(), kind.product_id(), serial)?;

        Ok(StreamDeck {
            kind,
            device,
            image_cache: RwLock::new(vec![]),
        })
    }
}

/// Instance methods of the struct
impl StreamDeck {
    /// Returns kind of the Stream Deck
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Returns manufacturer string of the device
    pub fn manufacturer(&self) -> Result<String, StreamDeckError> {
        Ok(self.device.get_manufacturer_string()?.unwrap_or_else(|| "Unknown".to_string()))
    }

    /// Returns product string of the device
    pub fn product(&self) -> Result<String, StreamDeckError> {
        Ok(self.device.get_product_string()?.unwrap_or_else(|| "Unknown".to_string()))
    }

    /// Returns serial number of the device
    pub fn serial_number(&self) -> Result<String, StreamDeckError> {
        match self.kind {
            Kind::Original | Kind::Mini => {
                let bytes = get_feature_report(&self.device, 0x03, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            Kind::MiniMk2 | Kind::MiniMk2Module => {
                let bytes = get_feature_report(&self.device, 0x03, 32)?;
                Ok(extract_str(&bytes[5..])?)
            }

            _ => {
                let bytes = get_feature_report(&self.device, 0x06, 32)?;
                Ok(extract_str(&bytes[2..])?)
            }
        }
        .map(|s| s.replace('\u{0001}', ""))
    }

    /// Returns firmware version of the StreamDeck
    pub fn firmware_version(&self) -> Result<String, StreamDeckError> {
        match self.kind {
            Kind::Original | Kind::Mini | Kind::MiniMk2 => {
                let bytes = get_feature_report(&self.device, 0x04, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            Kind::MiniMk2Module => {
                let bytes = get_feature_report(&self.device, 0xA1, 17)?;
                Ok(extract_str(&bytes[5..])?)
            }

            _ => {
                let bytes = get_feature_report(&self.device, 0x05, 32)?;
                Ok(extract_str(&bytes[6..])?)
            }
        }
    }

    /// Reads all possible input from Stream Deck device
    pub fn read_input(&self, timeout: Option<Duration>) -> Result<StreamDeckInput, StreamDeckError> {
        match &self.kind {
            Kind::Plus => {
                let data = read_data(&self.device, 14.max(5 + self.kind.encoder_count() as usize), timeout)?;

                if data[0] == 0 {
                    return Ok(StreamDeckInput::NoData);
                }

                match &data[1] {
                    0x0 => Ok(StreamDeckInput::ButtonStateChange(read_button_states(&self.kind, &data))),

                    0x2 => Ok(read_lcd_input(&data)?),

                    0x3 => Ok(read_encoder_input(&self.kind, &data)?),

                    _ => Err(StreamDeckError::BadData),
                }
            }

            _ => {
                let data = match self.kind {
                    Kind::Original | Kind::Mini | Kind::MiniMk2 | Kind::MiniMk2Module => read_data(&self.device, 1 + self.kind.key_count() as usize, timeout),
                    _ => read_data(&self.device, 4 + self.kind.key_count() as usize + self.kind.touchpoint_count() as usize, timeout),
                }?;

                if data[0] == 0 {
                    return Ok(StreamDeckInput::NoData);
                }

                Ok(StreamDeckInput::ButtonStateChange(read_button_states(&self.kind, &data)))
            }
        }
    }

    /// Resets the device
    pub fn reset(&self) -> Result<(), StreamDeckError> {
        match self.kind {
            Kind::Original | Kind::Mini | Kind::MiniMk2 | Kind::MiniMk2Module => {
                let mut buf = vec![0x0B, 0x63];

                buf.extend(vec![0u8; 15]);

                Ok(send_feature_report(&self.device, buf.as_slice())?)
            }

            _ => {
                let mut buf = vec![0x03, 0x02];

                buf.extend(vec![0u8; 30]);

                Ok(send_feature_report(&self.device, buf.as_slice())?)
            }
        }
    }

    /// Sets brightness of the device, value range is 0 - 100
    pub fn set_brightness(&self, percent: u8) -> Result<(), StreamDeckError> {
        let percent = percent.clamp(0, 100);

        match self.kind {
            Kind::Original | Kind::Mini | Kind::MiniMk2 | Kind::MiniMk2Module => {
                let mut buf = vec![0x05, 0x55, 0xaa, 0xd1, 0x01, percent];

                buf.extend(vec![0u8; 11]);

                Ok(send_feature_report(&self.device, buf.as_slice())?)
            }

            _ => {
                let mut buf = vec![0x03, 0x08, percent];

                buf.extend(vec![0u8; 29]);

                Ok(send_feature_report(&self.device, buf.as_slice())?)
            }
        }
    }

    fn send_image(&self, key: u8, image_data: &[u8]) -> Result<(), StreamDeckError> {
        if key >= self.kind.key_count() {
            return Err(StreamDeckError::InvalidKeyIndex);
        }

        let key = if let Kind::Original = self.kind { flip_key_index(&self.kind, key) } else { key };

        if !self.kind.is_visual() {
            return Err(StreamDeckError::NoScreen);
        }

        self.write_image_data_reports(
            image_data,
            WriteImageParameters::for_key(self.kind, image_data.len()),
            |page_number, this_length, last_package| match self.kind {
                Kind::Original => vec![0x02, 0x01, (page_number + 1) as u8, 0, if last_package { 1 } else { 0 }, key + 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],

                Kind::Mini | Kind::MiniMk2 | Kind::MiniMk2Module => vec![0x02, 0x01, page_number as u8, 0, if last_package { 1 } else { 0 }, key + 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],

                _ => vec![
                    0x02,
                    0x07,
                    key,
                    if last_package { 1 } else { 0 },
                    (this_length & 0xff) as u8,
                    (this_length >> 8) as u8,
                    (page_number & 0xff) as u8,
                    (page_number >> 8) as u8,
                ],
            },
        )?;
        Ok(())
    }

    /// Writes image data to Stream Deck device, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub fn write_image(&self, key: u8, image_data: &[u8]) -> Result<(), StreamDeckError> {
        let cache_entry = ImageCache { key, image_data: image_data.to_vec() };

        self.image_cache.write()?.push(cache_entry);

        Ok(())
    }

    /// Writes image data to Stream Deck device's lcd strip/screen as region.
    /// Only Stream Deck Plus supports writing LCD regions, for Stream Deck Neo use write_lcd_fill
    pub fn write_lcd(&self, x: u16, y: u16, rect: &ImageRect) -> Result<(), StreamDeckError> {
        match self.kind {
            Kind::Plus => (),
            _ => return Err(StreamDeckError::UnsupportedOperation),
        }

        self.write_image_data_reports(
            rect.data.as_slice(),
            WriteImageParameters {
                image_report_length: 1024,
                image_report_payload_length: 1024 - 16,
            },
            |page_number, this_length, last_package| {
                vec![
                    0x02,
                    0x0c,
                    (x & 0xff) as u8,
                    (x >> 8) as u8,
                    (y & 0xff) as u8,
                    (y >> 8) as u8,
                    (rect.w & 0xff) as u8,
                    (rect.w >> 8) as u8,
                    (rect.h & 0xff) as u8,
                    (rect.h >> 8) as u8,
                    if last_package { 1 } else { 0 },
                    (page_number & 0xff) as u8,
                    (page_number >> 8) as u8,
                    (this_length & 0xff) as u8,
                    (this_length >> 8) as u8,
                    0,
                ]
            },
        )
    }

    /// Writes image data to Stream Deck device's lcd strip/screen as full fill
    ///
    /// You can convert your images into proper image_data like this:
    /// ```
    /// use elgato_streamdeck::images::convert_image_with_format;
    /// let image_data = convert_image_with_format(device.kind().lcd_image_format(), image).unwrap();
    /// device.write_lcd_fill(&image_data);
    /// ```
    pub fn write_lcd_fill(&self, image_data: &[u8]) -> Result<(), StreamDeckError> {
        match self.kind {
            Kind::Neo => self.write_image_data_reports(
                image_data,
                WriteImageParameters {
                    image_report_length: 1024,
                    image_report_payload_length: 1024 - 8,
                },
                |page_number, this_length, last_package| {
                    vec![
                        0x02,
                        0x0b,
                        0,
                        if last_package { 1 } else { 0 },
                        (this_length & 0xff) as u8,
                        (this_length >> 8) as u8,
                        (page_number & 0xff) as u8,
                        (page_number >> 8) as u8,
                    ]
                },
            ),

            Kind::Plus => {
                let (w, h) = self.kind.lcd_strip_size().unwrap();

                self.write_image_data_reports(
                    image_data,
                    WriteImageParameters {
                        image_report_length: 1024,
                        image_report_payload_length: 1024 - 16,
                    },
                    |page_number, this_length, last_package| {
                        vec![
                            0x02,
                            0x0c,
                            0,
                            0,
                            0,
                            0,
                            (w & 0xff) as u8,
                            (w >> 8) as u8,
                            (h & 0xff) as u8,
                            (h >> 8) as u8,
                            if last_package { 1 } else { 0 },
                            (page_number & 0xff) as u8,
                            (page_number >> 8) as u8,
                            (this_length & 0xff) as u8,
                            (this_length >> 8) as u8,
                            0,
                        ]
                    },
                )
            }

            _ => Err(StreamDeckError::UnsupportedOperation),
        }
    }

    /// Sets button's image to blank, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub fn clear_button_image(&self, key: u8) -> Result<(), StreamDeckError> {
        self.send_image(key, &self.kind.blank_image())
    }

    /// Sets blank images to every button, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub fn clear_all_button_images(&self) -> Result<(), StreamDeckError> {
        for i in 0..self.kind.key_count() {
            self.clear_button_image(i)?
        }
        Ok(())
    }

    /// Sets specified button's image, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub fn set_button_image(&self, key: u8, image: DynamicImage) -> Result<(), StreamDeckError> {
        let image_data = convert_image(self.kind, image)?;
        self.write_image(key, &image_data)?;
        Ok(())
    }

    /// Sets specified touch point's led strip color
    pub fn set_touchpoint_color(&self, point: u8, red: u8, green: u8, blue: u8) -> Result<(), StreamDeckError> {
        if point >= self.kind.touchpoint_count() {
            return Err(StreamDeckError::InvalidTouchPointIndex);
        }

        let mut buf = vec![0x03, 0x06];

        let touchpoint_index: u8 = point + self.kind.key_count();
        buf.extend(vec![touchpoint_index]);
        buf.extend(vec![red, green, blue]);

        Ok(send_feature_report(&self.device, buf.as_slice())?)
    }

    /// Flushes the button's image to the device
    pub fn flush(&self) -> Result<(), StreamDeckError> {
        if self.image_cache.read()?.is_empty() {
            return Ok(());
        }

        for image in self.image_cache.read()?.iter() {
            self.send_image(image.key, &image.image_data)?;
        }

        self.image_cache.write()?.clear();

        Ok(())
    }

    /// Returns button state reader for this device
    pub fn get_reader(self: &Arc<Self>) -> Arc<DeviceStateReader> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(DeviceStateReader {
            device: self.clone(),
            states: Mutex::new(DeviceState {
                buttons: vec![false; self.kind.key_count() as usize + self.kind.touchpoint_count() as usize],
                encoders: vec![false; self.kind.encoder_count() as usize],
            }),
        })
    }

    fn write_image_data_reports<T>(&self, image_data: &[u8], parameters: WriteImageParameters, header_fn: T) -> Result<(), StreamDeckError>
    where
        T: Fn(usize, usize, bool) -> Vec<u8>,
    {
        let image_report_length = parameters.image_report_length;
        let image_report_payload_length = parameters.image_report_payload_length;

        let mut page_number = 0;
        let mut bytes_remaining = image_data.len();

        while bytes_remaining > 0 {
            let this_length = bytes_remaining.min(image_report_payload_length);
            let bytes_sent = page_number * image_report_payload_length;

            // Selecting header based on device
            let mut buf: Vec<u8> = header_fn(page_number, this_length, this_length == bytes_remaining);

            buf.extend(&image_data[bytes_sent..bytes_sent + this_length]);

            // Adding padding
            buf.extend(vec![0u8; image_report_length - buf.len()]);

            write_data(&self.device, &buf)?;

            bytes_remaining -= this_length;
            page_number += 1;
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
struct WriteImageParameters {
    pub image_report_length: usize,
    pub image_report_payload_length: usize,
}

impl WriteImageParameters {
    pub fn for_key(kind: Kind, image_data_len: usize) -> Self {
        let image_report_length = match kind {
            Kind::Original => 8191,
            _ => 1024,
        };

        let image_report_header_length = match kind {
            Kind::Original | Kind::Mini | Kind::MiniMk2 | Kind::MiniMk2Module => 16,
            _ => 8,
        };

        let image_report_payload_length = match kind {
            Kind::Original => image_data_len / 2,
            _ => image_report_length - image_report_header_length,
        };

        Self {
            image_report_length,
            image_report_payload_length,
        }
    }
}

/// Errors that can occur while working with Stream Decks
#[derive(Debug)]
pub enum StreamDeckError {
    /// HidApi error
    HidError(HidError),

    /// Failed to convert bytes into string
    Utf8Error(Utf8Error),

    /// Failed to encode image
    ImageError(ImageError),

    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    /// Tokio join error
    JoinError(tokio::task::JoinError),

    /// Reader mutex was poisoned
    PoisonError,

    /// There's literally nowhere to write the image
    NoScreen,

    /// Key index is invalid
    InvalidKeyIndex,

    /// Key index is invalid
    InvalidTouchPointIndex,

    /// Unrecognized Product ID
    UnrecognizedPID,

    /// The device doesn't support doing that
    UnsupportedOperation,

    /// Stream Deck sent unexpected data
    BadData,
}

impl Display for StreamDeckError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for StreamDeckError {}

impl From<HidError> for StreamDeckError {
    fn from(e: HidError) -> Self {
        Self::HidError(e)
    }
}

impl From<Utf8Error> for StreamDeckError {
    fn from(e: Utf8Error) -> Self {
        Self::Utf8Error(e)
    }
}

impl From<ImageError> for StreamDeckError {
    fn from(e: ImageError) -> Self {
        Self::ImageError(e)
    }
}

#[cfg(feature = "async")]
impl From<tokio::task::JoinError> for StreamDeckError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::JoinError(e)
    }
}

impl<T> From<PoisonError<T>> for StreamDeckError {
    fn from(_value: PoisonError<T>) -> Self {
        Self::PoisonError
    }
}

/// Tells what changed in button states
#[derive(Copy, Clone, Debug, Hash)]
pub enum DeviceStateUpdate {
    /// Button got pressed down
    ButtonDown(u8),

    /// Button got released
    ButtonUp(u8),

    /// Encoder got pressed down
    EncoderDown(u8),

    /// Encoder was released from being pressed down
    EncoderUp(u8),

    /// Encoder was twisted
    EncoderTwist(u8, i8),

    /// Touch Point got pressed down
    TouchPointDown(u8),

    /// Touch Point got released
    TouchPointUp(u8),

    /// Touch screen received short press
    TouchScreenPress(u16, u16),

    /// Touch screen received long press
    TouchScreenLongPress(u16, u16),

    /// Touch screen received a swipe
    TouchScreenSwipe((u16, u16), (u16, u16)),
}

#[derive(Default)]
struct DeviceState {
    /// Buttons include Touch Points state
    pub buttons: Vec<bool>,
    pub encoders: Vec<bool>,
}

/// Button reader that keeps state of the Stream Deck and returns events instead of full states
pub struct DeviceStateReader {
    device: Arc<StreamDeck>,
    states: Mutex<DeviceState>,
}

impl DeviceStateReader {
    /// Reads states and returns updates
    pub fn read(&self, timeout: Option<Duration>) -> Result<Vec<DeviceStateUpdate>, StreamDeckError> {
        let input = self.device.read_input(timeout)?;
        let mut my_states = self.states.lock()?;

        let mut updates = vec![];

        match input {
            StreamDeckInput::ButtonStateChange(buttons) => {
                for (index, (their, mine)) in zip(buttons.iter(), my_states.buttons.iter()).enumerate() {
                    if their != mine {
                        let key_count = self.device.kind.key_count();
                        if index < key_count as usize {
                            if *their {
                                updates.push(DeviceStateUpdate::ButtonDown(index as u8));
                            } else {
                                updates.push(DeviceStateUpdate::ButtonUp(index as u8));
                            }
                        } else if *their {
                            updates.push(DeviceStateUpdate::TouchPointDown(index as u8 - key_count));
                        } else {
                            updates.push(DeviceStateUpdate::TouchPointUp(index as u8 - key_count));
                        }
                    }
                }

                my_states.buttons = buttons;
            }

            StreamDeckInput::EncoderStateChange(encoders) => {
                for (index, (their, mine)) in zip(encoders.iter(), my_states.encoders.iter()).enumerate() {
                    if *their != *mine {
                        if *their {
                            updates.push(DeviceStateUpdate::EncoderDown(index as u8));
                        } else {
                            updates.push(DeviceStateUpdate::EncoderUp(index as u8));
                        }
                    }
                }

                my_states.encoders = encoders;
            }

            StreamDeckInput::EncoderTwist(twist) => {
                for (index, change) in twist.iter().enumerate() {
                    if *change != 0 {
                        updates.push(DeviceStateUpdate::EncoderTwist(index as u8, *change));
                    }
                }
            }

            StreamDeckInput::TouchScreenPress(x, y) => {
                updates.push(DeviceStateUpdate::TouchScreenPress(x, y));
            }

            StreamDeckInput::TouchScreenLongPress(x, y) => {
                updates.push(DeviceStateUpdate::TouchScreenLongPress(x, y));
            }

            StreamDeckInput::TouchScreenSwipe(s, e) => {
                updates.push(DeviceStateUpdate::TouchScreenSwipe(s, e));
            }

            _ => {}
        }

        drop(my_states);

        Ok(updates)
    }
}
