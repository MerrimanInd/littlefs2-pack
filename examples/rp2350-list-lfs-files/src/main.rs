//! Lists files from a LittleFS image on the RP2350
//!
//! Output is sent over USB CDC serial — open a terminal any time:
//!
//!   screen /dev/ttyACM0 115200
//!   # or
//!   minicom -D /dev/ttyACM0
//!
//! The file tree prints continuously every 5 seconds, so you can attach
//! a terminal whenever and immediately see output.
//!
//! LED signals (GPIO7 on Adafruit Feather RP2350):
//!   fast blink  → waiting for serial terminal
//!   1 blink     → terminal connected
//!   2 blinks    → LFS mounted
//!   3 blinks    → tree walk complete
//!   slow blink  → done, parked
//!   rapid blink → error

#![no_std]
#![no_main]

use core::fmt::Write as FmtWrite;
use embedded_hal::digital::OutputPin;
use rp235x_hal::clocks::init_clocks_and_plls;
use rp235x_hal::{self as hal, entry};
use rp235x_hal::{pac, Clock};
use usb_device::{class_prelude::*, prelude::*};
use usbd_serial::SerialPort;

use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult, path, path::Path};

// ── Panic handler (no defmt needed) ─────────────────────────────────────

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        cortex_m::asm::wfi();
    }
}

/// Tell the Boot ROM about our application
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

// ── Generated LittleFS config from build.rs ─────────────────────────────

#[allow(unused)]
mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

// ── Flash-backed read-only Storage ──────────────────────────────────────

struct FlashStorage;

impl Storage for FlashStorage {
    type CACHE_SIZE = lfs_config::CacheSize;
    type LOOKAHEAD_SIZE = lfs_config::LookaheadSize;

    const READ_SIZE: usize = lfs_config::READ_SIZE;
    const WRITE_SIZE: usize = lfs_config::WRITE_SIZE;
    const BLOCK_SIZE: usize = lfs_config::BLOCK_SIZE;
    const BLOCK_COUNT: usize = lfs_config::BLOCK_COUNT;

    fn read(&mut self, off: usize, buf: &mut [u8]) -> LfsResult<usize> {
        let image = lfs_config::IMAGE;
        for (i, b) in buf.iter_mut().enumerate() {
            let addr = off + i;
            *b = if addr < image.len() {
                image[addr]
            } else {
                0xFF
            };
        }
        Ok(buf.len())
    }

    fn write(&mut self, _off: usize, _data: &[u8]) -> LfsResult<usize> {
        Err(littlefs2::io::Error::IO)
    }

    fn erase(&mut self, _off: usize, _len: usize) -> LfsResult<usize> {
        Err(littlefs2::io::Error::IO)
    }
}

// ── Formatting helpers for USB serial output ────────────────────────────

/// Fixed-size buffer that implements core::fmt::Write
struct FmtBuf {
    buf: [u8; 512],
    pos: usize,
}

impl FmtBuf {
    fn new() -> Self {
        Self {
            buf: [0; 512],
            pos: 0,
        }
    }

    fn reset(&mut self) {
        self.pos = 0;
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.pos]
    }
}

impl FmtWrite for FmtBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let space = self.buf.len() - self.pos;
        let n = bytes.len().min(space);
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        if n < bytes.len() {
            Err(core::fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// Write raw bytes to USB serial, polling until all bytes are sent
fn usb_write_bytes(
    serial: &mut SerialPort<hal::usb::UsbBus>,
    usb_dev: &mut UsbDevice<hal::usb::UsbBus>,
    data: &[u8],
) {
    let mut pos = 0;
    while pos < data.len() {
        usb_dev.poll(&mut [serial]);
        match serial.write(&data[pos..]) {
            Ok(n) => pos += n,
            Err(_) => {}
        }
    }
    usb_dev.poll(&mut [serial]);
}

/// Format and write a line to USB serial (with \r\n)
fn usb_println(
    serial: &mut SerialPort<hal::usb::UsbBus>,
    usb_dev: &mut UsbDevice<hal::usb::UsbBus>,
    buf: &mut FmtBuf,
    args: core::fmt::Arguments<'_>,
) {
    buf.reset();
    core::fmt::write(buf, args).ok();
    usb_write_bytes(serial, usb_dev, buf.as_bytes());
    usb_write_bytes(serial, usb_dev, b"\r\n");
}

// ── Recursive directory listing ─────────────────────────────────────────

const MAX_DEPTH: usize = 8;

fn list_tree(
    fs: &Filesystem<'_, FlashStorage>,
    dir_path: &Path,
    depth: usize,
    serial: &mut SerialPort<hal::usb::UsbBus>,
    usb_dev: &mut UsbDevice<hal::usb::UsbBus>,
    buf: &mut FmtBuf,
) {
    if depth >= MAX_DEPTH {
        return;
    }

    fs.read_dir_and_then(dir_path, |dir| {
        for entry in dir {
            let entry = entry?;
            let name = entry.file_name();
            if name == path!(".") || name == path!("..") {
                continue;
            }

            let child_path = dir_path.join(name);

            if entry.file_type().is_dir() {
                usb_println(
                    serial,
                    usb_dev,
                    buf,
                    format_args!("[{}] DIR  {}/", depth, child_path),
                );
                list_tree(fs, &child_path, depth + 1, serial, usb_dev, buf);
            } else {
                usb_println(
                    serial,
                    usb_dev,
                    buf,
                    format_args!(
                        "[{}] FILE {} ({} bytes)",
                        depth,
                        child_path,
                        entry.metadata().len()
                    ),
                );
            }
        }
        Ok(())
    })
    .ok();
}

// ── LED helpers ─────────────────────────────────────────────────────────

fn blink_n<P: OutputPin>(led: &mut P, delay: &mut cortex_m::delay::Delay, n: u32) {
    for _ in 0..n {
        let _ = led.set_high();
        delay.delay_ms(200);
        let _ = led.set_low();
        delay.delay_ms(200);
    }
    delay.delay_ms(600);
}

// ── Entry point ─────────────────────────────────────────────────────────

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let core = cortex_m::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);
    let sio = hal::Sio::new(pac.SIO);

    let external_xtal_freq_hz = 12_000_000u32;
    let clocks = init_clocks_and_plls(
        external_xtal_freq_hz,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let mut delay = cortex_m::delay::Delay::new(core.SYST, clocks.system_clock.freq().to_Hz());

    let pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let mut led = pins.gpio7.into_push_pull_output();

    // ── Set up USB serial ──
    let usb_bus = UsbBusAllocator::new(hal::usb::UsbBus::new(
        pac.USB,
        pac.USB_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));

    let mut serial = SerialPort::new(&usb_bus);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x2e8a, 0x000a))
        .device_class(usbd_serial::USB_CLASS_CDC)
        .build();

    // ── Brief wait for USB to enumerate ──
    // Output loops continuously, so no need to wait long
    for i in 0..300 {
        usb_dev.poll(&mut [&mut serial]);
        if serial.dtr() {
            break;
        }
        if i % 25 == 0 {
            let _ = led.set_high();
        } else if i % 25 == 12 {
            let _ = led.set_low();
        }
        delay.delay_ms(10);
    }
    let _ = led.set_low();
    delay.delay_ms(200);

    let mut buf = FmtBuf::new();

    // ── Stage 1: connected ──
    blink_n(&mut led, &mut delay, 1);

    usb_println(
        &mut serial,
        &mut usb_dev,
        &mut buf,
        format_args!("LittleFS tree listing - RP2350"),
    );
    usb_println(
        &mut serial,
        &mut usb_dev,
        &mut buf,
        format_args!(
            "block_size={} block_count={} read={} write={}",
            lfs_config::BLOCK_SIZE,
            lfs_config::BLOCK_COUNT,
            lfs_config::READ_SIZE,
            lfs_config::WRITE_SIZE,
        ),
    );
    usb_println(
        &mut serial,
        &mut usb_dev,
        &mut buf,
        format_args!(
            "image_len={} total_size={}",
            lfs_config::IMAGE.len(),
            lfs_config::TOTAL_SIZE,
        ),
    );

    let mut storage = FlashStorage;
    let mut alloc = Filesystem::allocate();

    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(fs) => {
            usb_println(
                &mut serial,
                &mut usb_dev,
                &mut buf,
                format_args!("Mounted!"),
            );
            blink_n(&mut led, &mut delay, 2);

            // Print the tree on repeat — attach a terminal any time to see it
            let mut iteration: u32 = 0;
            loop {
                iteration += 1;
                usb_println(
                    &mut serial,
                    &mut usb_dev,
                    &mut buf,
                    format_args!("--- tree listing #{} ---", iteration),
                );

                list_tree(&fs, path!("/"), 0, &mut serial, &mut usb_dev, &mut buf);

                usb_println(
                    &mut serial,
                    &mut usb_dev,
                    &mut buf,
                    format_args!("--- end (next in 5s) ---"),
                );

                // 5 second pause, keep USB alive + blink LED
                for _ in 0..500 {
                    delay.delay_ms(10);
                    usb_dev.poll(&mut [&mut serial]);
                }
                // Quick blink to show we're still alive
                let _ = led.set_high();
                delay.delay_ms(100);
                let _ = led.set_low();
            }
        }
        Err(e) => {
            usb_println(
                &mut serial,
                &mut usb_dev,
                &mut buf,
                format_args!("Mount failed: {:?}", e),
            );
            loop {
                let _ = led.set_high();
                delay.delay_ms(50);
                let _ = led.set_low();
                delay.delay_ms(50);
                usb_dev.poll(&mut [&mut serial]);
            }
        }
    }
}

/// Program metadata for `picotool info`
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [rp235x_hal::binary_info::EntryAddr; 5] = [
    rp235x_hal::binary_info::rp_cargo_bin_name!(),
    rp235x_hal::binary_info::rp_cargo_version!(),
    rp235x_hal::binary_info::rp_program_description!(c"LittleFS tree listing"),
    rp235x_hal::binary_info::rp_cargo_homepage_url!(),
    rp235x_hal::binary_info::rp_program_build_attribute!(),
];
