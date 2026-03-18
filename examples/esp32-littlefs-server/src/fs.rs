use core::cell::RefCell;
use critical_section::Mutex;
use defmt::info;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_println::{self as _, println};
use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult};

extern crate alloc;
use alloc::vec::Vec;

// ── Generated LittleFS config from build.rs ─────────────────────────────

#[allow(unused)]
pub mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
    include!(concat!(env!("OUT_DIR"), "/partition_config.rs"));
}

// ── Flash-backed Storage impl ───────────────────────────────────────────

pub struct FlashLfsStorage<'a> {
    flash: esp_storage::FlashStorage<'a>,
    offset: u32,
}

impl<'a> FlashLfsStorage<'a> {
    fn new(flash: esp_hal::peripherals::FLASH<'a>, offset: u32) -> Self {
        Self {
            flash: esp_storage::FlashStorage::new(flash),
            offset,
        }
    }
}

impl Storage for FlashLfsStorage<'_> {
    type CACHE_SIZE = lfs_config::CacheSize;
    type LOOKAHEAD_SIZE = lfs_config::LookaheadSize;

    const READ_SIZE: usize = lfs_config::READ_SIZE;
    const WRITE_SIZE: usize = lfs_config::WRITE_SIZE;
    const BLOCK_SIZE: usize = lfs_config::BLOCK_SIZE;
    const BLOCK_COUNT: usize = lfs_config::BLOCK_COUNT;

    fn read(&mut self, off: usize, buf: &mut [u8]) -> LfsResult<usize> {
        ReadNorFlash::read(&mut self.flash, self.offset + off as u32, buf)
            .map_err(|_| littlefs2::io::Error::IO)?;
        Ok(buf.len())
    }

    fn write(&mut self, off: usize, data: &[u8]) -> LfsResult<usize> {
        NorFlash::write(&mut self.flash, self.offset + off as u32, data)
            .map_err(|_| littlefs2::io::Error::IO)?;
        Ok(data.len())
    }

    fn erase(&mut self, off: usize, len: usize) -> LfsResult<usize> {
        NorFlash::erase(
            &mut self.flash,
            self.offset + off as u32,
            self.offset + (off + len) as u32,
        )
        .map_err(|_| littlefs2::io::Error::IO)?;
        Ok(len)
    }
}

// ── Path helper ─────────────────────────────────────────────────────────

fn make_null_terminated(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

unsafe fn bytes_as_path(bytes: &[u8]) -> &littlefs2::path::Path {
    unsafe { &*(bytes as *const [u8] as *const littlefs2::path::Path) }
}

// ── Named helper for reading file contents ──────────────────────────────

fn read_file_contents<S: Storage>(file: &littlefs2::fs::File<'_, '_, S>) -> LfsResult<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

// ── Global mounted filesystem ───────────────────────────────────────────

static MOUNTED: Mutex<RefCell<bool>> = Mutex::new(RefCell::new(false));
static mut FS_PTR: Option<*mut ()> = None;

/// Mount the LittleFS filesystem. Call once at startup.
/// No files are loaded — zero heap cost.
pub fn mount_fs(flash: esp_hal::peripherals::FLASH) {
    info!(
        "Mounting LittleFS from flash @ {:#X}...",
        lfs_config::PARTITION_OFFSET,
    );

    // Extend the FLASH peripheral lifetime to 'static.
    // Safety: we leak the storage below so it lives forever,
    // and the FLASH peripheral is never used elsewhere.
    let flash: esp_hal::peripherals::FLASH<'static> = unsafe { core::mem::transmute(flash) };

    let storage = alloc::boxed::Box::leak(alloc::boxed::Box::new(FlashLfsStorage::new(
        flash,
        lfs_config::PARTITION_OFFSET,
    )));
    let alloc = alloc::boxed::Box::leak(alloc::boxed::Box::new(Filesystem::allocate()));

    match Filesystem::mount(alloc, storage) {
        Ok(fs) => {
            println!("Mounted!");
            let fs_leaked = alloc::boxed::Box::leak(alloc::boxed::Box::new(fs));
            unsafe {
                FS_PTR = Some(fs_leaked as *mut _ as *mut ());
            }
            critical_section::with(|cs| {
                *MOUNTED.borrow_ref_mut(cs) = true;
            });
            info!("LittleFS mounted, ready for on-demand reads");
        }
        Err(e) => {
            panic!("Mount failed: {:?}", e);
        }
    }
}

/// Read a file from the mounted filesystem on-demand.
/// Returns None if the file doesn't exist or FS isn't mounted.
///
/// The returned Vec lives on the heap (internal SRAM by default for
/// small allocations). It is TEMPORARY — the caller should drop it
/// after writing the response, so the memory is reclaimed.
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    let is_mounted = critical_section::with(|cs| *MOUNTED.borrow_ref(cs));
    if !is_mounted {
        return None;
    }

    let path_bytes = make_null_terminated(path);
    let lfs_path = unsafe { bytes_as_path(&path_bytes) };

    let fs: &Filesystem<'_, FlashLfsStorage<'_>> =
        unsafe { &*(FS_PTR.unwrap() as *const Filesystem<'_, FlashLfsStorage<'_>>) };

    match fs.open_file_and_then(lfs_path, &mut read_file_contents::<FlashLfsStorage>) {
        Ok(data) => Some(data),
        Err(_) => None,
    }
}

/// Build a LittleFS path from a URL path segment.
/// Normalizes: strips leading slash, defaults "" to "index.html",
/// then prepends "/" for LittleFS.
///
/// Returns None if the path looks suspicious (e.g. contains "..").
pub fn normalize_url_path(url_path: &str) -> Option<Vec<u8>> {
    let trimmed = url_path.trim_start_matches('/');

    // Basic path traversal protection
    if trimmed.contains("..") {
        return None;
    }

    let file_path = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    // LittleFS paths start with "/"
    let mut full = Vec::with_capacity(1 + file_path.len());
    full.push(b'/');
    full.extend_from_slice(file_path.as_bytes());

    Some(full)
}

/// Guess a Content-Type from the file extension.
pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}
