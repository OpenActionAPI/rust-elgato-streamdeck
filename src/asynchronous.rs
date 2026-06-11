//! Code from this module is using [block_in_place](tokio::task::block_in_place),
//! and so they cannot be used in [current_thread](tokio::runtime::Builder::new_current_thread) runtimes

use std::iter::zip;
use std::sync::Arc;
use std::time::Duration;

use hidapi::{HidApi, HidResult};
use image::DynamicImage;
use tokio::sync::Mutex;
use tokio::task::block_in_place;
use tokio::time::sleep;

use crate::{DeviceState, DeviceStateUpdate, Kind, list_devices, StreamDeck, StreamDeckError, StreamDeckInput};
use crate::images::{convert_image_async, ImageRect};

/// Actually refreshes the device list, can be safely ran inside [multi_thread](tokio::runtime::Builder::new_multi_thread) runtime
pub fn refresh_device_list_async(hidapi: &mut HidApi) -> HidResult<()> {
    block_in_place(move || hidapi.refresh_devices())
}

/// Returns a list of devices as (Kind, Serial Number) that could be found using HidApi,
/// can be safely ran inside [multi_thread](tokio::runtime::Builder::new_multi_thread) runtime
///
/// **WARNING:** To refresh the list, use [refresh_device_list]
pub fn list_devices_async(hidapi: &HidApi) -> Vec<(Kind, String)> {
    block_in_place(move || list_devices(hidapi))
}

/// Stream Deck interface suitable to be used in async, uses [block_in_place](block_in_place)
/// so this wrapper cannot be used in [current_thread](tokio::runtime::Builder::new_current_thread) runtimes
#[derive(Clone)]
pub struct AsyncStreamDeck {
    kind: Kind,
    device: Arc<Mutex<StreamDeck>>,
}

/// Static functions of the struct
impl AsyncStreamDeck {
    /// Attempts to connect to the device, can be safely ran inside [multi_thread](tokio::runtime::Builder::new_multi_thread) runtime
    pub fn connect(hidapi: &HidApi, kind: Kind, serial: &str) -> Result<AsyncStreamDeck, StreamDeckError> {
        let device = block_in_place(move || StreamDeck::connect(hidapi, kind, serial))?;

        Ok(AsyncStreamDeck {
            kind,
            device: Arc::new(Mutex::new(device)),
        })
    }
}

impl From<StreamDeck> for AsyncStreamDeck {
    /// Wraps an already-opened [StreamDeck], allowing the caller to control which thread performs the HID open
    fn from(device: StreamDeck) -> AsyncStreamDeck {
        AsyncStreamDeck {
            kind: device.kind(),
            device: Arc::new(Mutex::new(device)),
        }
    }
}

/// Instance methods of the struct
impl AsyncStreamDeck {
    /// Returns kind of the Stream Deck
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Returns manufacturer string of the device
    pub async fn manufacturer(&self) -> Result<String, StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.manufacturer())
    }

    /// Returns product string of the device
    pub async fn product(&self) -> Result<String, StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.product())
    }

    /// Returns serial number of the device
    pub async fn serial_number(&self) -> Result<String, StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.serial_number())
    }

    /// Returns firmware version of the StreamDeck
    pub async fn firmware_version(&self) -> Result<String, StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.firmware_version())
    }

    /// Reads button states, awaits until there's data.
    /// Poll rate determines how often button state gets checked
    pub async fn read_input(&self, poll_rate: f32) -> Result<StreamDeckInput, StreamDeckError> {
        loop {
            let device = self.device.lock().await;
            let data = block_in_place(move || device.read_input(None))?;

            if !data.is_empty() {
                return Ok(data);
            }

            sleep(Duration::from_secs_f32(1.0 / poll_rate)).await;
        }
    }

    /// Resets the device
    pub async fn reset(&self) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.reset())
    }

    /// Sets brightness of the device, value range is 0 - 100
    pub async fn set_brightness(&self, percent: u8) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.set_brightness(percent))
    }

    /// Writes image data to Stream Deck device, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub async fn write_image(&self, key: u8, image_data: &[u8]) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.write_image(key, image_data))
    }

    /// Writes image data to Stream Deck device's lcd strip/screen as region.
    /// Only Stream Deck Plus supports writing LCD regions, for Stream Deck Neo use write_lcd_fill
    pub async fn write_lcd(&self, x: u16, y: u16, rect: &ImageRect) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.write_lcd(x, y, rect))
    }

    /// Writes image data to Stream Deck device's lcd strip/screen as full fill
    ///
    /// You can convert your images into proper image_data like this:
    /// ```
    /// use elgato_streamdeck::images::{convert_image_with_format_async};
    /// let image_data = convert_image_with_format_async(device.kind().lcd_image_format(), image).await.unwrap();
    /// device.write_lcd_fill(&image_data).await;
    /// ```
    pub async fn write_lcd_fill(&self, image_data: &[u8]) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.write_lcd_fill(image_data))
    }

    /// Sets button's image to blank, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub async fn clear_button_image(&self, key: u8) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.clear_button_image(key))
    }

    /// Sets blank images to every button, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub async fn clear_all_button_images(&self) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.clear_all_button_images())
    }

    /// Sets specified button's image, changes must be flushed with `.flush()` before
    /// they will appear on the device!
    pub async fn set_button_image(&self, key: u8, image: DynamicImage) -> Result<(), StreamDeckError> {
        let image = convert_image_async(self.kind, image)?;

        let device = self.device.lock().await;
        block_in_place(move || device.write_image(key, &image))
    }

    /// Sets specified touch point's led strip color
    pub async fn set_touchpoint_color(&self, point: u8, red: u8, green: u8, blue: u8) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.set_touchpoint_color(point, red, green, blue))
    }

    /// Flushes the button's image to the device
    pub async fn flush(&self) -> Result<(), StreamDeckError> {
        let device = self.device.lock().await;
        block_in_place(move || device.flush())
    }

    /// Returns button state reader for this device
    pub fn get_reader(&self) -> Arc<AsyncDeviceStateReader> {
        Arc::new(AsyncDeviceStateReader {
            device: self.clone(),
            states: Mutex::new(DeviceState {
                buttons: vec![false; self.kind.key_count() as usize + self.kind.touchpoint_count() as usize],
                encoders: vec![false; self.kind.encoder_count() as usize],
            }),
        })
    }
}

/// Button reader that keeps state of the Stream Deck and returns events instead of full states
pub struct AsyncDeviceStateReader {
    device: AsyncStreamDeck,
    states: Mutex<DeviceState>,
}

impl AsyncDeviceStateReader {
    /// Reads states and returns updates
    pub async fn read(&self, poll_rate: f32) -> Result<Vec<DeviceStateUpdate>, StreamDeckError> {
        let input = self.device.read_input(poll_rate).await?;
        let mut my_states = self.states.lock().await;

        let mut updates = vec![];

        match input {
            StreamDeckInput::ButtonStateChange(buttons) => {
                for (index, (their, mine)) in zip(buttons.iter(), my_states.buttons.iter()).enumerate() {
                    if *their != *mine {
                        if index < self.device.kind.key_count() as usize {
                            if *their {
                                updates.push(DeviceStateUpdate::ButtonDown(index as u8));
                            } else {
                                updates.push(DeviceStateUpdate::ButtonUp(index as u8));
                            }
                        } else if *their {
                            updates.push(DeviceStateUpdate::TouchPointDown(index as u8 - self.device.kind.key_count()));
                        } else {
                            updates.push(DeviceStateUpdate::TouchPointUp(index as u8 - self.device.kind.key_count()));
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
