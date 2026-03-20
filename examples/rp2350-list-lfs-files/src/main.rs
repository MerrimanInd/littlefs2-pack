//! Lists files from a LittleFS image on the RP2350
//!
//! The LFS image is embedded in flash via `include_bytes!` in the build
//! script. Since flash is XIP-mapped, we read directly from it — no RAM
//! buffer needed at all.

#![no_std]
#![no_main]

use defmt::{error, info, Debug2Format, Display2Format};
use defmt_rtt as _;
use panic_probe as _;
use rp235x_hal::clocks::init_clocks_and_plls;
use rp235x_hal::pac;
use rp235x_hal::{self as hal, entry};

use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult, path, path::Path};

/// Tell the Boot ROM about our application
#[unsafe(link_section = ".start_block")]
#[used]
pub static IMAGE_DEF: hal::block::ImageDef = hal::block::ImageDef::secure_exe();

// ── Generated LittleFS config from build.rs ─────────────────────────────

#[allow(unused)]
mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

// ── Flash-backed read-only Storage impl ─────────────────────────────────
//
// The LFS image lives in .rodata (XIP-mapped flash). We read directly
// from it — zero SRAM cost. Writes and erases return errors since this
// is read-only.

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
        if off + buf.len() <= image.len() {
            buf.copy_from_slice(&image[off..off + buf.len()]);
        } else {
            // Reading past the image into the unwritten region — return 0xFF
            // (erased flash) for bytes beyond the image.
            for (i, b) in buf.iter_mut().enumerate() {
                let addr = off + i;
                *b = if addr < image.len() {
                    image[addr]
                } else {
                    0xFF
                };
            }
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

// ── Recursive directory listing ─────────────────────────────────────────

const MAX_DEPTH: usize = 8;

fn list_tree(fs: &Filesystem<'_, FlashStorage>, dir_path: &Path, depth: usize) {
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
                info!("[{=usize}] DIR  {}/", depth, Display2Format(&child_path));
                list_tree(fs, &child_path, depth + 1);
            } else {
                info!(
                    "[{=usize}] FILE {} ({=usize} bytes)",
                    depth,
                    Display2Format(&child_path),
                    entry.metadata().len(),
                );
            }
        }
        Ok(())
    })
    .ok();
}

// ── Entry point ─────────────────────────────────────────────────────────

#[entry]
fn main() -> ! {
    info!("LittleFS tree listing - RP2350");

    let mut pac = pac::Peripherals::take().unwrap();
    let _core = cortex_m::Peripherals::take().unwrap();
    let mut watchdog = hal::Watchdog::new(pac.WATCHDOG);
    let sio = hal::Sio::new(pac.SIO);

    let external_xtal_freq_hz = 12_000_000u32;
    let _clocks = init_clocks_and_plls(
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

    let _pins = hal::gpio::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    info!(
        "LFS config: block_size={=usize} block_count={=usize} read={=usize} write={=usize}",
        lfs_config::BLOCK_SIZE,
        lfs_config::BLOCK_COUNT,
        lfs_config::READ_SIZE,
        lfs_config::WRITE_SIZE,
    );
    info!(
        "image_len={=usize} total_size={=usize}",
        lfs_config::IMAGE.len(),
        lfs_config::TOTAL_SIZE,
    );

    let mut storage = FlashStorage;
    let mut alloc = Filesystem::allocate();

    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(fs) => {
            info!("Mounted!");
            list_tree(&fs, path!("/"), 0);
        }
        Err(e) => {
            error!("Mount failed: {:?}", Debug2Format(&e));
        }
    }

    info!("Done.");

    loop {
        cortex_m::asm::wfi();
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
