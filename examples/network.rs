#[cfg(not(all(feature = "async", feature = "tcp")))]
compile_error!("The `async` and `tcp` features must be enabled to compile this example.");

use std::time::Duration;

use elgato_streamdeck::transport::DEFAULT_TCP_PORT;
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate};
use image::open;

/// Drives a Stream Deck attached to a Network Dock over the CORA TCP protocol.
///
/// Usage: `cargo run --features async,tcp --example network -- <dock-ip>[:port]`
/// (port defaults to 5343). Press the last button to quit.
#[tokio::main]
async fn main() {
    // Resolve the dock address from argv, defaulting the port to 5343.
    let mut addr = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: network <dock-ip>[:port]");
        std::process::exit(2);
    });
    if !addr.contains(':') {
        addr = format!("{addr}:{DEFAULT_TCP_PORT}");
    }

    println!("Connecting to dock at {addr} ...");

    // Connect and auto-detect the attached device's Kind from its vendor/product id.
    let device = AsyncStreamDeck::connect_network(&addr).expect("Failed to connect to dock");
    let kind = device.kind();

    println!(
        "Connected to {:?} '{}' (firmware '{}')",
        kind,
        device.serial_number().await.unwrap_or_else(|_| "unknown".into()),
        device.firmware_version().await.unwrap_or_else(|_| "unknown".into()),
    );

    device.set_brightness(50).await.unwrap();
    device.clear_all_button_images().await.unwrap();

    // Draw an image on every key.
    if kind.is_visual() {
        let image = open("examples/no-place-like-localhost.jpg").unwrap();
        for key in 0..kind.key_count() {
            device.set_button_image(key, image.clone()).await.unwrap();
        }
        device.flush().await.unwrap();
    }

    println!("Ready — press the last button ({}) to quit.", kind.key_count().saturating_sub(1));

    // Read button/encoder events until the last button is released.
    let reader = device.get_reader();
    'outer: loop {
        let updates = match reader.read(100.0).await {
            Ok(updates) => updates,
            Err(e) => {
                eprintln!("read error (disconnected?): {e}");
                break;
            }
        };

        for update in updates {
            match update {
                DeviceStateUpdate::ButtonDown(key) => println!("Button {key} down"),
                DeviceStateUpdate::ButtonUp(key) => {
                    println!("Button {key} up");
                    if key == kind.key_count().saturating_sub(1) {
                        break 'outer;
                    }
                }
                DeviceStateUpdate::EncoderTwist(dial, ticks) => println!("Dial {dial} twisted by {ticks}"),
                DeviceStateUpdate::EncoderDown(dial) => println!("Dial {dial} down"),
                DeviceStateUpdate::EncoderUp(dial) => println!("Dial {dial} up"),
                DeviceStateUpdate::TouchPointDown(point) => println!("Touch point {point} down"),
                DeviceStateUpdate::TouchPointUp(point) => println!("Touch point {point} up"),
                DeviceStateUpdate::TouchScreenPress(x, y) => println!("Touch screen press at {x}, {y}"),
                DeviceStateUpdate::TouchScreenLongPress(x, y) => println!("Touch screen long press at {x}, {y}"),
                DeviceStateUpdate::TouchScreenSwipe((sx, sy), (ex, ey)) => {
                    println!("Touch screen swipe from {sx}, {sy} to {ex}, {ey}")
                }
            }
        }
    }

    drop(reader);
    let _ = device.reset().await;
    // Give the reset a moment to flush before the socket closes on drop.
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("Done.");
}
