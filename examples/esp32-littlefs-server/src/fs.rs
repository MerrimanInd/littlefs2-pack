use defmt::info;
use embedded_storage::nor_flash::ReadNorFlash;
use esp_println::{self as _, println};
use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult};

extern crate alloc;
use alloc::vec;

// ── Generated LittleFS config from build.rs ─────────────────────────────
// NOTE: Your build.rs should still generate the geometry constants
// (BLOCK_SIZE, BLOCK_COUNT, TOTAL_SIZE, etc.) but no longer needs to
// embed the IMAGE bytes via include_bytes!().

#[allow(unused)]
pub mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

// ── Flash partition where the LittleFS image lives ──────────────────────
// Must match the offset and size in partitions.csv
const LITTLEFS_PARTITION_OFFSET: u32 = 0x20_0000; // 4 MB into flash

// ── RAM-backed Storage impl ─────────────────────────────────────────────
// Instantiated from the generated littlefs constants
struct RamStorage<'a> {
    buf: &'a mut [u8],
}

impl Storage for RamStorage<'_> {
    type CACHE_SIZE = lfs_config::CacheSize;
    type LOOKAHEAD_SIZE = lfs_config::LookaheadSize;

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

// ── File system mounter ─────────────────────────────

/// Reads the LittleFS image from flash into PSRAM, then mounts it.
///
/// Requires `esp_storage` in Cargo.toml:
///   esp-storage = { version = "0.4", features = ["esp32s3"] }
pub fn mount_fs(flash: esp_hal::peripherals::FLASH) {
    // Allocate the full filesystem buffer in PSRAM
    let mut storage_buf = vec![0u8; lfs_config::TOTAL_SIZE];

    // Read the LittleFS image from the flash partition into PSRAM
    let mut flash_storage = esp_storage::FlashStorage::new(flash);

    // Read in 4 KiB chunks (sector-aligned) to stay within read limits
    const CHUNK: usize = 4096;
    let total = lfs_config::TOTAL_SIZE;
    let mut offset: usize = 0;

    info!(
        "Reading {} bytes of LittleFS image from flash @ {:#X}...",
        total, LITTLEFS_PARTITION_OFFSET
    );

    while offset < total {
        let end = (offset + CHUNK).min(total);
        flash_storage
            .read(
                LITTLEFS_PARTITION_OFFSET + offset as u32,
                &mut storage_buf[offset..end],
            )
            .expect("Flash read failed");
        offset = end;
    }

    info!("Flash read complete, mounting filesystem...");

    // Create an instance of the RamStorage backed by the PSRAM buffer
    let mut storage = RamStorage {
        buf: &mut storage_buf,
    };

    let mut alloc = Filesystem::allocate();

    // Mount the filesystem
    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(_fs) => {
            println!("Mounted!");
        }
        Err(e) => {
            println!("Mount failed: {:?}", e);
        }
    }
}
