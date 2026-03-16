use esp_println::{self as _, println};
use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult};

extern crate alloc;
use alloc::vec;

// ── Generated LittleFS config from build.rs ─────────────────────────────

#[allow(unused)]
pub mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

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

pub fn mount_fs() {
    // Allocate in PSRAM and copy the image in
    let mut storage_buf = vec![0u8; lfs_config::TOTAL_SIZE];
    storage_buf[..lfs_config::IMAGE.len()].copy_from_slice(lfs_config::IMAGE);

    // Create an instance of the RamStorage and copy the image in
    let mut storage = RamStorage {
        buf: &mut storage_buf,
    };

    let mut alloc = Filesystem::allocate();

    // Mount the actual
    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(_fs) => {
            println!("Mounted!");
        }
        Err(e) => {
            println!("Mount failed: {:?}", e);
        }
    }
}
