# Examples

The example projects here show how to integrate these LittleFS tools in embedded projects.

## ESP Examples
The ESP32 examples were tested on an [Unexpected Maker FeatherS3 board](https://esp32s3.com/feathers3.html) to take advantage of the 16MB Flash chip.

The flake provides a devShell for ESP development. Enter it with:

```bash
nix develop .#esp
```
If you don't have `nix` or are on Windows you'll have to follow the installation instructions in [the Rust on ESP book](https://docs.espressif.com/projects/rust/book/). Because the ESP S-series use Xtensa processors there's a separate toolchain required.


### `esp32-list-lfs-files`
This simple example builds a directory, adds it directly to the firmware image with the `include_bytes!()` macro, and then reads the contents out to the terminal.

### `esp32-littlefs-server`
This is the most full-featured example, using all of the LittleFS tools in the repo. It builds a local website containing a simple website into a LittleFS image using `builds.rs`. It flashes both firmware and filesystem with the `littlefs flash` utility. Then firmware image then loads it from the partition and serves it on a WiFi network.

Note that you'll have to set your device to a static IP address on the same subnet as 192.168.13.37 as the example doesn't have a DHCP server. Then access the website at that IP address.

## Raspberry Pi Examples
The RP2350 examples were tested on an [Adafruit RP2350 with 8MB PSRAM](https://www.adafruit.com/product/6130).

RP2350s have first-party Rust support so don't require as much set up as the ESP chips. But the flake also provides a Raspberry Pi devShell. Enter it with:

```bash
nix develop .#rp
```

### `rp2350-list-lfs-files`
This version brings the same functionality to the Raspberry Pi RP2350. Since most RP2350 boards don't have an onboard debugger this repurposes the USB port as a serial port. Flash the image, then attach to the port with a serial reader like PuTTY or minicom, and the chip will broadcast the image contents every few seconds.
