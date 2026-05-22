use std::sync::Arc;
use std::time::Duration;

use image::open;

use elgato_streamdeck::info::{ImageFormat};
use elgato_streamdeck::{DeviceStateUpdate, list_devices, new_hidapi, StreamDeck};
use elgato_streamdeck::images::{convert_image_with_format, ImageRect};

fn main() {
    // Create instance of HidApi
    match new_hidapi() {
        Ok(hid) => {
            // Refresh device list
            for (kind, serial) in list_devices(&hid) {
                println!("{:?} {} {}", kind, serial, kind.product_id());

                // Connect to the device
                let device = StreamDeck::connect(&hid, kind, &serial).expect("Failed to connect");
                // Print out some info from the device
                println!("Connected to '{}' with version '{}'", device.serial_number().unwrap(), device.firmware_version().unwrap());

                device.set_brightness(50).unwrap();
                device.clear_all_button_images().unwrap();
                // Use image-rs to load an image
                let image = open("examples/no-place-like-localhost.jpg").unwrap();

                println!("Key count: {}", kind.key_count());
                // Write it to the device
                for i in 0..kind.key_count() {
                    device.set_button_image(i, image.clone()).unwrap();
                }

                println!("Touch point count: {}", kind.touchpoint_count());
                for i in 0..kind.touchpoint_count() {
                    device.set_touchpoint_color(i, 255, 255, 255).unwrap();
                }

                if let Some(format) = device.kind().lcd_image_format() {
                    let converted_image = convert_image_with_format(format, image.clone()).unwrap();
                    let _ = device.write_lcd_fill(&converted_image);
                }

                let small = match device.kind().lcd_strip_size() {
                    Some((w, h)) => {
                        let min = w.min(h) as u32;
                        let converted_image = convert_image_with_format(
                            ImageFormat {
                                mode: device.kind().lcd_image_format().unwrap().mode,
                                size: (min.try_into().unwrap(), min.try_into().unwrap()),
                                rotation: device.kind().lcd_image_format().unwrap().rotation,
                                mirror: device.kind().lcd_image_format().unwrap().mirror,
                            },
                            image.clone(),
                        )
                        .unwrap();
                        Some(ImageRect::from_image(image::load_from_memory(&converted_image).unwrap()).unwrap())
                    }
                    None => None,
                };

                // Flush
                device.flush().unwrap();

                #[allow(clippy::arc_with_non_send_sync)]
                let device = Arc::new(device);
                {
                    let reader = device.get_reader();

                    'infinite: loop {
                        let updates = match reader.read(Some(Duration::from_secs_f64(100.0))) {
                            Ok(updates) => updates,
                            Err(_) => break,
                        };
                        for update in updates {
                            match update {
                                DeviceStateUpdate::ButtonDown(key) => {
                                    println!("Button {} down", key);
                                }
                                DeviceStateUpdate::ButtonUp(key) => {
                                    println!("Button {} up", key);
                                    if key == device.kind().key_count() - 1 {
                                        break 'infinite;
                                    }
                                }
                                DeviceStateUpdate::EncoderTwist(dial, ticks) => {
                                    println!("Dial {} twisted by {}", dial, ticks);
                                }
                                DeviceStateUpdate::EncoderDown(dial) => {
                                    println!("Dial {} down", dial);
                                }
                                DeviceStateUpdate::EncoderUp(dial) => {
                                    println!("Dial {} up", dial);
                                }

                                DeviceStateUpdate::TouchPointDown(point) => {
                                    println!("Touch point {} down", point);
                                }
                                DeviceStateUpdate::TouchPointUp(point) => {
                                    println!("Touch point {} up", point);
                                }

                                DeviceStateUpdate::TouchScreenPress(x, y) => {
                                    println!("Touch Screen press at {x}, {y}");
                                    if let Some(small) = &small {
                                        device.write_lcd(x, y, small).unwrap();
                                    }
                                }

                                DeviceStateUpdate::TouchScreenLongPress(x, y) => {
                                    println!("Touch Screen long press at {x}, {y}")
                                }

                                DeviceStateUpdate::TouchScreenSwipe((sx, sy), (ex, ey)) => {
                                    println!("Touch Screen swipe from {sx}, {sy} to {ex}, {ey}")
                                }
                            }
                        }
                    }

                    drop(reader);
                }
            }
        }
        Err(e) => eprintln!("Failed to create HidApi instance: {}", e),
    }
}
