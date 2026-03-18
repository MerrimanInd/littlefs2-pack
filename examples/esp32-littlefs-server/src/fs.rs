use defmt::info;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_println::{self as _, println};
use littlefs2::{driver::Storage, fs::Filesystem, io::Result as LfsResult};

extern crate alloc;
use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};

// ── Generated LittleFS config from build.rs ─────────────────────────────

#[allow(unused)]
pub mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}

// ── Flash partition where the LittleFS image lives ──────────────────────
const LITTLEFS_PARTITION_OFFSET: u32 = 0x20_0000; // 2 MB into flash

// ── Flash-backed Storage impl ───────────────────────────────────────────

struct FlashLfsStorage<'a> {
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
// littlefs2::path::Path is repr(transparent) over null-terminated bytes
// and only implements TryFrom for fixed-size byte arrays, not &str.
// We construct null-terminated bytes and transmute.

fn make_null_terminated(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

/// Convert a null-terminated byte slice to a littlefs2 Path.
///
/// # Safety
/// `bytes` must be a valid null-terminated byte sequence with no
/// interior null bytes. This matches Path's internal representation.
unsafe fn bytes_as_path(bytes: &[u8]) -> &littlefs2::path::Path {
    unsafe { &*(bytes as *const [u8] as *const littlefs2::path::Path) }
}

// ── Static file server ──────────────────────────────────────────────────

pub struct FileServer {
    files: BTreeMap<&'static str, &'static [u8]>,
}

impl FileServer {
    pub fn get(&self, path: &str) -> Option<&'static [u8]> {
        self.files.get(path).copied()
    }

    pub fn get_str(&self, path: &str) -> Option<&'static str> {
        self.get(path)
            .map(|bytes| core::str::from_utf8(bytes).expect("file is not valid UTF-8"))
    }

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
}

// ── Named helper functions (avoids HRTB closure issues) ─────────────────

/// Collect directory entries as (name, is_dir) pairs.
/// Using a named function instead of a closure fixes the higher-ranked
/// lifetime bounds that littlefs2's `read_dir_and_then` requires.
fn collect_dir_entries<S: Storage>(
    iter: &mut littlefs2::fs::ReadDir<'_, '_, S>,
) -> LfsResult<Vec<(String, bool)>> {
    let mut entries = Vec::new();
    for entry in iter {
        let entry: littlefs2::fs::DirEntry = entry?;
        let name = entry.file_name();

        // Convert Path -> bytes -> str (strip trailing null if present)
        let name_bytes = name.as_str().as_bytes(); // Path derefs to [u8]
        let name_str = core::str::from_utf8(name_bytes)
            .expect("filename is not valid UTF-8")
            .trim_end_matches('\0');

        if name_str == "." || name_str == ".." {
            continue;
        }

        let is_dir = entry.file_type().is_dir();
        entries.push((String::from(name_str), is_dir));
    }
    Ok(entries)
}

/// Read a file's full contents into a Vec.
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

// ── Directory walker (iterative, not recursive in closures) ─────────────

fn walk_all_files<S: Storage>(fs: &Filesystem<S>) -> Vec<String> {
    let mut file_paths = Vec::new();
    // Stack of directories to visit
    let mut dir_stack: Vec<String> = Vec::new();
    dir_stack.push(String::from("/"));

    while let Some(dir_path) = dir_stack.pop() {
        let path_bytes = make_null_terminated(&dir_path);
        let lfs_path = unsafe { bytes_as_path(&path_bytes) };

        let entries = fs
            .read_dir_and_then(lfs_path, &mut collect_dir_entries::<S>)
            .unwrap_or_default();

        for (name, is_dir) in entries {
            let full_path = if dir_path == "/" {
                alloc::format!("/{}", name)
            } else {
                alloc::format!("{}/{}", dir_path, name)
            };

            if is_dir {
                dir_stack.push(full_path);
            } else {
                file_paths.push(full_path);
            }
        }
    }

    file_paths
}

// ── File system mounter ─────────────────────────────────────────────────

pub fn mount_fs(flash: esp_hal::peripherals::FLASH) -> &'static FileServer {
    let mut storage = FlashLfsStorage::new(flash, LITTLEFS_PARTITION_OFFSET);
    let mut alloc = Filesystem::allocate();

    info!(
        "Mounting LittleFS from flash @ {:#X}...",
        LITTLEFS_PARTITION_OFFSET
    );

    match Filesystem::mount(&mut alloc, &mut storage) {
        Ok(fs) => {
            println!("Mounted!");

            // Walk the entire filesystem (iterative, avoids HRTB issues)
            let file_paths = walk_all_files(&fs);

            info!("Found {} files in filesystem", file_paths.len());

            // Read each file's contents from flash and leak into 'static
            let mut files = BTreeMap::new();
            for path_string in file_paths {
                let path_bytes = make_null_terminated(&path_string);
                let lfs_path = unsafe { bytes_as_path(&path_bytes) };

                let result =
                    fs.open_file_and_then(lfs_path, &mut read_file_contents::<FlashLfsStorage>);

                match result {
                    Ok(data) => {
                        let size = data.len();
                        let static_path: &'static str = Box::leak(path_string.into_boxed_str());
                        let static_data: &'static [u8] = Box::leak(data.into_boxed_slice());
                        files.insert(static_path, static_data);
                        info!("  {} ({} bytes)", static_path, size);
                    }
                    Err(e) => {
                        println!("  Failed to read {}: {:?}", path_string, e);
                    }
                }
            }

            // fs, alloc, storage all dropped here — flash is released,
            // only the extracted file contents remain in PSRAM.
            let server = Box::leak(Box::new(FileServer { files }));
            info!("FileServer ready with {} files", server.files.len());
            server
        }
        Err(e) => {
            panic!("Mount failed: {:?}", e);
        }
    }
}
