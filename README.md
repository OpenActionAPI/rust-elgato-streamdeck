# elgato-streamdeck

Rust library for interacting with Elgato Stream Deck hardware.
Heavily based on [python-elgato-streamdeck](https://github.com/abcminiuser/python-elgato-streamdeck) and partially on
[rust-streamdeck](https://github.com/ryankurte/rust-streamdeck).

This library was made by [@TheJebForge](https://github.com/TheJebForge) as a better-designed alternative to rust-streamdeck, taking code from both of the libraries and making it more pleasant to use.

## udev rules for Linux

If you're using systemd on your system, you might have to install udev rules to allow connecting to devices from userspace.

You can do that by using the following command to copy this repo's included `40-streamdeck.rules` file into `udev/rules.d/`:

```shell
cp 40-streamdeck.rules /etc/udev/rules.d/
```

And then reloading udev rules:

```shell
sudo udevadm control --reload-rules
```

Unplugging and plugging back in the device should also help.

You should also create a group called "plugdev" if it doesn't exist and add yourself to it, so you get access to the devices.
You also need to restart the user session to let user group changes to kick in.

## Example

```rust
use elgato_streamdeck::{new_hidapi, StreamDeck};

// Create instance of HidApi
let hid = new_hidapi();

// List devices and unsafely take first one
let (kind, serial) = StreamDeck::list_devices(&hid).remove(0);

// Connect to the device
let mut device = StreamDeck::connect(&hid, kind, &serial)
    .expect("Failed to connect");

// Print out some info from the device
println!(
    "Connected to '{}' with version '{}'",
    device.serial_number().unwrap(),
    device.firmware_version().unwrap()
);

// Set device brightness
device.set_brightness(35).unwrap();

// Use image-rs to load an image
let image = image::open("no-place-like-localhost.jpg").unwrap();

// Write it to the device
device.set_button_image(7, image).unwrap();

// Flush
if device.updated {
    device.flush().unwrap();
}
```

## Status

- [x] Convenient to use API for looking up devices, connecting to them and interacting with them
- [x] Reading buttons with async

## Supported Devices

As it stands, this library should support the following devices.

- Stream Deck Original
- Stream Deck Original V2
- Stream Deck XL
- Stream Deck XL V2
- Stream Deck Mini
- Stream Deck Mini Discord Edition
- Stream Deck Mini Mk2
- Stream Deck Mk2
- Stream Deck Pedal
- Stream Deck Plus (thanks to [node-elgato-stream-deck](https://github.com/Julusian/node-elgato-stream-deck))
- Stream Deck Neo (thanks to [@ejiektpobehuk](https://github.com/ejiektpobehuk), [@AkechiShiro](https://github.com/AkechiShiro) and [node-elgato-stream-deck](https://github.com/Julusian/node-elgato-stream-deck))

Support for non-Elgato (Mirabox/Ajazz) devices has been removed in v0.11. If you wish to use these devices, please use the `mirajazz` crate, or use v0.10.2 of this crate.
