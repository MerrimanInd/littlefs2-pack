#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;
use alloc::vec;

use esp_alloc as _;
use esp_hal::{
    clock::CpuClock,
    main,
    time::{Duration, Instant},
};
use esp_println::{self as _, println};

use generic_array::typenum;
use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult, path, path::Path};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {}", info);
    loop {}
}

// ── Generated LittleFS config from build.rs ─────────────────────────────
mod littlefs2_generated {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

use littlefs2_generated as lfs_config;

// ── LittleFS image embedded at build time ───────────────────────────────
static LFS_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/filesystem.bin"));

const TOTAL_SIZE: usize = lfs_config::BLOCK_SIZE * lfs_config::BLOCK_COUNT;

// ── RAM-backed Storage impl ─────────────────────────────────────────────
struct RamStorage<'a> {
    buf: &'a mut [u8],
}

impl Storage for RamStorage<'_> {
    type CACHE_SIZE = typenum::U256;
    type LOOKAHEAD_SIZE = typenum::U1;

    const READ_SIZE: usize = lfs_config::READ_SIZE;
    const WRITE_SIZE: usize = lfs_config::WRITE_SIZE;
    const BLOCK_SIZE: usize = lfs_config::BLOCK_SIZE;
    const BLOCK_COUNT: usize = lfs_config::BLOCK_COUNT;

    fn read(&mut self, off: usize, buf: &mut [u8]) -> LfsResult<usize> {
        buf.copy_from_slice(&self.buf[off..off + buf.len()]);
        Ok(buf.len())
    }

    fn write(&mut self, off: usize, data: &[u8]) -> LfsResult<usize> {
        self.buf[off..off + data.len()].copy_from_slice(data);
        Ok(data.len())
    }

    fn erase(&mut self, off: usize, len: usize) -> LfsResult<usize> {
        for byte in &mut self.buf[off..off + len] {
            *byte = 0xFF;
        }
        Ok(len)
    }
}

// ── Recursive tree printer with box-drawing characters ──────────────────
// Avoids heap allocation — uses a fixed-size prefix buffer on the stack.
const MAX_DEPTH: usize = 8;
const PREFIX_BUF_SIZE: usize = MAX_DEPTH * 7; // "│   " is up to 7 bytes in UTF-8

fn print_tree(fs: &Filesystem<'_, RamStorage<'_>>, dir_path: &Path, prefix: &[u8], depth: usize) {
    if depth >= MAX_DEPTH {
        return;
    }

    // First pass: count entries so we know which is last
    let mut total = 0usize;
    fs.read_dir_and_then(dir_path, |dir| {
        for entry in dir {
            let entry = entry?;
            let name = entry.file_name();
            if name == path!(".") || name == path!("..") {
                continue;
            }
            total += 1;
        }
        Ok(())
    })
    .ok();

    // Second pass: print with connectors
    let mut index = 0usize;
    fs.read_dir_and_then(dir_path, |dir| {
        for entry in dir {
            let entry = entry?;
            let name = entry.file_name();
            if name == path!(".") || name == path!("..") {
                continue;
            }

            index += 1;
            let is_last = index == total;

            // Print: "{prefix}{connector}{name}"
            let prefix_str = core::str::from_utf8(prefix).unwrap_or("");
            let connector = if is_last { "└── " } else { "├── " };

            if entry.file_type().is_dir() {
                println!("{}{}{}/", prefix_str, connector, name);

                // Build child prefix by appending to current prefix
                let extension = if is_last { "    " } else { "│   " };
                let ext_bytes = extension.as_bytes();
                let new_len = prefix.len() + ext_bytes.len();

                if new_len <= PREFIX_BUF_SIZE {
                    let mut child_prefix = [0u8; PREFIX_BUF_SIZE];
                    child_prefix[..prefix.len()].copy_from_slice(prefix);
                    child_prefix[prefix.len()..new_len].copy_from_slice(ext_bytes);

                    let child_path = dir_path.join(name);
                    print_tree(fs, &child_path, &child_prefix[..new_len], depth + 1);
                }
            } else {
                println!(
                    "{}{}{} ({} bytes)",
                    prefix_str,
                    connector,
                    name,
                    entry.metadata().len()
                );
            }
        }
        Ok(())
    })
    .ok();
}

#[allow(clippy::large_stack_frames)]
#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // Set up internal SRAM heap — enables `alloc` crate
    esp_alloc::heap_allocator!(size: 72 * 1024);
    // Add PSRAM as heap region
    esp_alloc::psram_allocator!(&peripherals.PSRAM, esp_hal::psram);

    println!(
        "Copying LFS image ({} bytes) into PSRAM...",
        LFS_IMAGE.len()
    );

    // Allocate in PSRAM and copy the image in
    let mut storage_buf = vec![0u8; TOTAL_SIZE];
    storage_buf[..LFS_IMAGE.len()].copy_from_slice(LFS_IMAGE);

    println!(
        "block_size={} block_count={} read={} write={}",
        lfs_config::BLOCK_SIZE,
        lfs_config::BLOCK_COUNT,
        lfs_config::READ_SIZE,
        lfs_config::WRITE_SIZE,
    );
    println!("image_len={} total_size={}", LFS_IMAGE.len(), TOTAL_SIZE);

    let mut storage = RamStorage {
        buf: &mut storage_buf,
    };

    let mut alloc = Filesystem::allocate();

    // Mount the actual
    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(fs) => {
            println!("Mounted!");
            print_tree(&fs, path!("/"), &[], 0);
        }
        Err(e) => {
            println!("Mount failed: {:?}", e);
        }
    }

    println!("\nDone.");

    loop {
        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(5000) {}
    }
}
