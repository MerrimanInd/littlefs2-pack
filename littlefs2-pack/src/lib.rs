//! # littlefs2-pack
//!
//! A Rust crate for building a file system into a LittleFS
//! binary file to be flashed to an embedded device.
//!
//! This crate wraps the [LittleFS C library](https://github.com/littlefs-project/littlefs)
//! using the [`littlefs2-sys`](https://crates.io/crates/littlefs2-sys) crate. The [`littlefs2`](https://crates.io/crates/littlefs2) crate
//! might have been an easier starting point but it doesn't
//! currently allow setting the image configuration dynamically
//!  at runtime, such as the block size and count.
//!
//! `littlefs2-pack` is tested for compatibility with the C++
//! [`mklittlefs` project](https://github.com/earlephilhower/mklittlefs). This is ensured with the `cross-compat.rs`
//! test that packs with one tool then unpack with the other,
//! in both directions. These tests are ran against the version of
//! `mklittlefs` in the submodule and requires that tool to be built
//! prior to running the tests.
//!
//! ## Example
//!
//! ```rust,no_run
//! use littlefs2_pack::LfsImage;
//! use littlefs2_config::ImageConfig;
//!
//! let config = ImageConfig::from(
//!     4096, // block_size
//!     256,  // block_count, 1 MiB total
//!     256, // read_size
//!     256, // write_size
//! );
//!
//! let mut image = LfsImage::new(config).unwrap();
//! image.format().unwrap();
//!
//! image.mount_and_then(|fs| {
//!     fs.create_dir("/data")?;
//!     fs.write_file("/data/hello.txt", b"Hello from LittleFS!")?;
//!     Ok(())
//! }).unwrap();
//!
//! let binary = image.into_data();
//! std::fs::write("filesystem.bin", &binary).unwrap();
//! ```

use std::ffi::{CString, c_int, c_void};
use std::ptr;

use littlefs2_config::ImageConfig;
use littlefs2_sys as lfs;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by LittleFS operations.
#[derive(Debug, thiserror::Error)]
pub enum LfsError {
    #[error("LittleFS error: {0} (code {1})")]
    Lfs(String, i32),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("I/O error: {0}")]
    Io(String),

    #[error("Path contains interior NUL byte")]
    NulPath,
}

impl LfsError {
    fn from_lfs_error(code: i32) -> Self {
        let msg = match code {
            x if x == lfs::lfs_error_LFS_ERR_IO => "I/O error",
            x if x == lfs::lfs_error_LFS_ERR_CORRUPT => "Corrupted",
            x if x == lfs::lfs_error_LFS_ERR_NOENT => "No such file or directory",
            x if x == lfs::lfs_error_LFS_ERR_EXIST => "Entry already exists",
            x if x == lfs::lfs_error_LFS_ERR_NOTDIR => "Not a directory",
            x if x == lfs::lfs_error_LFS_ERR_ISDIR => "Is a directory",
            x if x == lfs::lfs_error_LFS_ERR_NOTEMPTY => "Directory not empty",
            x if x == lfs::lfs_error_LFS_ERR_BADF => "Bad file number",
            x if x == lfs::lfs_error_LFS_ERR_FBIG => "File too large",
            x if x == lfs::lfs_error_LFS_ERR_INVAL => "Invalid parameter",
            x if x == lfs::lfs_error_LFS_ERR_NOSPC => "No space left on device",
            x if x == lfs::lfs_error_LFS_ERR_NOMEM => "No memory available",
            x if x == lfs::lfs_error_LFS_ERR_NOATTR => "No attribute found",
            x if x == lfs::lfs_error_LFS_ERR_NAMETOOLONG => "File name too long",
            _ => "Unknown error",
        };
        LfsError::Lfs(msg.to_string(), code)
    }
}

/// Check an lfs return code; Ok(()) on success, Err on negative codes.
fn check(code: c_int) -> Result<(), LfsError> {
    if code < 0 {
        Err(LfsError::from_lfs_error(code))
    } else {
        Ok(())
    }
}

/// Check and return the positive return value (e.g. bytes read/written).
fn check_positive(code: c_int) -> Result<usize, LfsError> {
    if code < 0 {
        Err(LfsError::from_lfs_error(code))
    } else {
        Ok(code as usize)
    }
}

/// Convert a Rust string path to a C string for the lfs API.
fn to_cpath(path: &str) -> Result<CString, LfsError> {
    CString::new(path).map_err(|_| LfsError::NulPath)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
/// Validate that the config values are acceptable to the LittleFS C library.
fn validate_for_lfs(config: &ImageConfig) -> Result<(), LfsError> {
    if config.block_size() < 128 {
        return Err(LfsError::InvalidConfig("block_size must be >= 128".into()));
    }
    if config.block_count() == 0 {
        return Err(LfsError::InvalidConfig("block_count must be > 0".into()));
    }
    if config.read_size() == 0 || config.write_size() == 0 {
        return Err(LfsError::InvalidConfig(
            "read_size and write_size must be > 0".into(),
        ));
    }
    if config.block_size() % config.read_size() != 0 {
        return Err(LfsError::InvalidConfig(
            "block_size must be a multiple of read_size".into(),
        ));
    }
    if config.block_size() % config.write_size() != 0 {
        return Err(LfsError::InvalidConfig(
            "block_size must be a multiple of write_size".into(),
        ));
    }
    Ok(())
}

/// Determine a good cache size for the LittleFS C config.
fn cache_size(config: &ImageConfig) -> usize {
    let base = config.read_size().max(config.write_size());
    if config.block_size() % base == 0 {
        base
    } else {
        config.block_size()
    }
}

/// Lookahead size in bytes — must be a multiple of 8.
fn lookahead_size(config: &ImageConfig) -> usize {
    let bytes_needed = (config.block_count() + 7) / 8;
    let aligned = ((bytes_needed + 7) / 8) * 8;
    aligned.max(16)
}

// ---------------------------------------------------------------------------
// LfsImage — an in-memory block device + LittleFS state
// ---------------------------------------------------------------------------

/// An in-memory LittleFS2 filesystem image.
///
/// Holds the raw byte buffer (the "flash") and the configuration needed to
/// operate on it with the littlefs C library.
pub struct LfsImage {
    /// The raw image data (simulated flash).
    data: Vec<u8>,

    /// Our configuration.
    config: ImageConfig,

    /// Heap-allocated read cache buffer.
    read_cache: Vec<u8>,
    /// Heap-allocated write cache buffer.
    write_cache: Vec<u8>,
    /// Heap-allocated lookahead buffer.
    lookahead_buf: Vec<u8>,
}

impl LfsImage {
    /// Create a new blank image, initialized to 0xFF (erased flash state).
    pub fn new(config: ImageConfig) -> Result<Self, LfsError> {
        validate_for_lfs(&config)?;
        let total = config.image_size();
        let cache_sz = cache_size(&config) as usize;
        let la_sz = lookahead_size(&config) as usize;

        Ok(LfsImage {
            data: vec![0xFF; total],
            read_cache: vec![0u8; cache_sz],
            write_cache: vec![0u8; cache_sz],
            lookahead_buf: vec![0u8; la_sz],
            config,
        })
    }

    /// Create an image from existing data (e.g. read from a .bin file).
    pub fn from_data(config: ImageConfig, data: Vec<u8>) -> Result<Self, LfsError> {
        validate_for_lfs(&config)?;
        let expected = config.image_size();
        if data.len() != expected {
            return Err(LfsError::InvalidConfig(format!(
                "data length ({}) doesn't match expected image size ({})",
                data.len(),
                expected
            )));
        }
        let cache_sz = cache_size(&config) as usize;
        let la_sz = lookahead_size(&config) as usize;

        Ok(LfsImage {
            data,
            read_cache: vec![0u8; cache_sz],
            write_cache: vec![0u8; cache_sz],
            lookahead_buf: vec![0u8; la_sz],
            config,
        })
    }

    /// Consume the image and return the raw data buffer.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Get a reference to the raw image data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get the configuration.
    pub fn config(&self) -> &ImageConfig {
        &self.config
    }

    // -- Internal: build the lfs_config struct pointing at our buffers ------

    /// Build an `lfs_config` that points back into `self` through a raw pointer.
    ///
    /// # Safety
    /// The returned config borrows `self` mutably through the `context` pointer.
    /// The caller must ensure `self` is not moved or dropped while the config
    /// is in use.
    unsafe fn build_lfs_config(&mut self) -> lfs::lfs_config {
        lfs::lfs_config {
            context: self as *mut LfsImage as *mut c_void,
            read: Some(Self::lfs_read),
            prog: Some(Self::lfs_prog),
            erase: Some(Self::lfs_erase),
            sync: Some(Self::lfs_sync),
            read_size: self.config.read_size() as u32,
            prog_size: self.config.write_size() as u32,
            block_size: self.config.block_size() as u32,
            block_count: self.config.block_count() as u32,
            block_cycles: -1, // disable wear leveling for image creation
            cache_size: cache_size(&self.config) as u32,
            lookahead_size: lookahead_size(&self.config) as u32,
            read_buffer: self.read_cache.as_mut_ptr() as *mut c_void,
            prog_buffer: self.write_cache.as_mut_ptr() as *mut c_void,
            lookahead_buffer: self.lookahead_buf.as_mut_ptr() as *mut c_void,
            name_max: 0, // use default (LFS_NAME_MAX)
            file_max: 0, // use default (LFS_FILE_MAX)
            attr_max: 0, // use default (LFS_ATTR_MAX)
            metadata_max: 0,
            inline_max: 0,
            compact_thresh: 0,
            disk_version: 0,
        }
    }

    // -- C callbacks --------------------------------------------------------

    /// Read callback for littlefs.
    extern "C" fn lfs_read(
        c: *const lfs::lfs_config,
        block: lfs::lfs_block_t,
        off: lfs::lfs_off_t,
        buffer: *mut c_void,
        size: lfs::lfs_size_t,
    ) -> c_int {
        unsafe {
            let image = &*((*c).context as *const LfsImage);
            let block_size = (*c).block_size;
            let start = (block * block_size + off) as usize;
            let len = size as usize;
            if start + len > image.data.len() {
                return lfs::lfs_error_LFS_ERR_IO;
            }
            ptr::copy_nonoverlapping(image.data.as_ptr().add(start), buffer as *mut u8, len);
            0
        }
    }

    /// Program (write) callback for littlefs.
    extern "C" fn lfs_prog(
        c: *const lfs::lfs_config,
        block: lfs::lfs_block_t,
        off: lfs::lfs_off_t,
        buffer: *const c_void,
        size: lfs::lfs_size_t,
    ) -> c_int {
        unsafe {
            let image = &mut *((*c).context as *mut LfsImage);
            let block_size = (*c).block_size;
            let start = (block * block_size + off) as usize;
            let len = size as usize;
            if start + len > image.data.len() {
                return lfs::lfs_error_LFS_ERR_IO;
            }
            ptr::copy_nonoverlapping(buffer as *const u8, image.data.as_mut_ptr().add(start), len);
            0
        }
    }

    /// Erase callback for littlefs. Sets erased blocks to 0xFF.
    extern "C" fn lfs_erase(c: *const lfs::lfs_config, block: lfs::lfs_block_t) -> c_int {
        unsafe {
            let image = &mut *((*c).context as *mut LfsImage);
            let block_size = (*c).block_size as usize;
            let start = block as usize * block_size;
            if start + block_size > image.data.len() {
                return lfs::lfs_error_LFS_ERR_IO;
            }
            for byte in &mut image.data[start..start + block_size] {
                *byte = 0xFF;
            }
            0
        }
    }

    /// Sync callback (no-op for RAM storage).
    extern "C" fn lfs_sync(_c: *const lfs::lfs_config) -> c_int {
        0
    }

    // -- High-level operations ----------------------------------------------

    /// Format the image as a fresh LittleFS2 filesystem.
    pub fn format(&mut self) -> Result<(), LfsError> {
        unsafe {
            let cfg = self.build_lfs_config();
            let mut state: lfs::lfs_t = std::mem::zeroed();
            check(lfs::lfs_format(&mut state, &cfg))
        }
    }

    /// Mount the filesystem, call the closure with a [`MountedFs`] handle,
    /// then unmount. This is the safe, closure-based API that guarantees the
    /// filesystem is always unmounted even on error.
    pub fn mount_and_then<F, T>(&mut self, f: F) -> Result<T, LfsError>
    where
        F: FnOnce(&MountedFs<'_>) -> Result<T, LfsError>,
    {
        unsafe {
            let cfg = self.build_lfs_config();
            let mut state: lfs::lfs_t = std::mem::zeroed();
            check(lfs::lfs_mount(&mut state, &cfg))?;

            let fs = MountedFs {
                state: &mut state,
                config: &cfg,
            };

            let result = f(&fs);

            // Always unmount, even if the closure returned an error
            let unmount_result = check(lfs::lfs_unmount(&mut state));

            // Return the closure error if it failed, otherwise the unmount error
            match result {
                Ok(val) => {
                    unmount_result?;
                    Ok(val)
                }
                Err(e) => Err(e),
            }
        }
    }

    /// Check whether the image contains a valid, mountable LittleFS2 filesystem.
    pub fn is_mountable(&mut self) -> bool {
        self.mount_and_then(|_| Ok(())).is_ok()
    }
}

// ---------------------------------------------------------------------------
// MountedFs — operations on a mounted filesystem
// ---------------------------------------------------------------------------

/// A handle to a mounted LittleFS2 filesystem.
///
/// Only obtained through [`LfsImage::mount_and_then`], which guarantees proper
/// mount/unmount lifecycle.
pub struct MountedFs<'a> {
    state: &'a mut lfs::lfs_t,
    config: &'a lfs::lfs_config,
}

/// An entry returned by [`MountedFs::read_dir`].
#[derive(Debug)]
pub struct DirEntry {
    pub name: String,
    pub size: usize,
    pub is_dir: bool,
}

impl<'a> MountedFs<'a> {
    /// Create a directory at the given path.
    pub fn create_dir(&self, path: &str) -> Result<(), LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            // lfs_mkdir takes *mut lfs_t despite not needing ownership semantics
            // beyond what the C library internally manages. We must cast away the
            // shared reference here because the closure-based API only gives us &self
            // (to mirror how littlefs2's Filesystem works with RefCell internally).
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            check(lfs::lfs_mkdir(state_ptr, cpath.as_ptr()))
        }
    }

    /// Recursively create directories along a path.
    pub fn create_dir_all(&self, path: &str) -> Result<(), LfsError> {
        let parts: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        let mut current = String::new();
        for part in parts {
            current.push('/');
            current.push_str(part);
            match self.create_dir(&current) {
                Ok(()) => {}
                Err(LfsError::Lfs(_, code)) if code == lfs::lfs_error_LFS_ERR_EXIST => {
                    // Directory already exists, that's fine
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Write a file at the given path, creating it (and truncating if it exists).
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            let mut file: lfs::lfs_file_t = std::mem::zeroed();

            // lfs_file_opencfg requires a caller-supplied cache buffer
            let cache_size = self.config.cache_size as usize;
            let mut file_cache = vec![0u8; cache_size];
            let mut file_cfg: lfs::lfs_file_config = std::mem::zeroed();
            file_cfg.buffer = file_cache.as_mut_ptr() as *mut c_void;

            let flags = lfs::lfs_open_flags_LFS_O_WRONLY
                | lfs::lfs_open_flags_LFS_O_CREAT
                | lfs::lfs_open_flags_LFS_O_TRUNC;

            check(lfs::lfs_file_opencfg(
                state_ptr,
                &mut file,
                cpath.as_ptr(),
                flags as i32,
                &file_cfg,
            ))?;

            let write_result = {
                let written = lfs::lfs_file_write(
                    state_ptr,
                    &mut file,
                    data.as_ptr() as *const c_void,
                    data.len() as u32,
                );
                if written < 0 {
                    Err(LfsError::from_lfs_error(written))
                } else if (written as usize) != data.len() {
                    Err(LfsError::Io(format!(
                        "short write: {} of {} bytes",
                        written,
                        data.len()
                    )))
                } else {
                    Ok(())
                }
            };

            // Always close the file
            let close_result = check(lfs::lfs_file_close(state_ptr, &mut file));
            write_result?;
            close_result
        }
    }

    /// Read the entire contents of a file.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            let mut file: lfs::lfs_file_t = std::mem::zeroed();

            // lfs_file_opencfg requires a caller-supplied cache buffer
            let cache_size = self.config.cache_size as usize;
            let mut file_cache = vec![0u8; cache_size];
            let mut file_cfg: lfs::lfs_file_config = std::mem::zeroed();
            file_cfg.buffer = file_cache.as_mut_ptr() as *mut c_void;

            let flags = lfs::lfs_open_flags_LFS_O_RDONLY;

            check(lfs::lfs_file_opencfg(
                state_ptr,
                &mut file,
                cpath.as_ptr(),
                flags as i32,
                &file_cfg,
            ))?;

            let result = (|| -> Result<Vec<u8>, LfsError> {
                // Get file size
                let size = lfs::lfs_file_size(state_ptr, &mut file);
                let size = check_positive(size)?;

                let mut buf = vec![0u8; size];
                if size > 0 {
                    let read = lfs::lfs_file_read(
                        state_ptr,
                        &mut file,
                        buf.as_mut_ptr() as *mut c_void,
                        size as u32,
                    );
                    let read = check_positive(read)?;
                    buf.truncate(read);
                }
                Ok(buf)
            })();

            let close_result = check(lfs::lfs_file_close(state_ptr, &mut file));
            let data = result?;
            close_result?;
            Ok(data)
        }
    }

    /// List entries in a directory (excluding "." and "..").
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            let mut dir: lfs::lfs_dir_t = std::mem::zeroed();

            check(lfs::lfs_dir_open(state_ptr, &mut dir, cpath.as_ptr()))?;

            let result = (|| -> Result<Vec<DirEntry>, LfsError> {
                let mut entries = Vec::new();
                loop {
                    let mut info: lfs::lfs_info = std::mem::zeroed();
                    let rc = lfs::lfs_dir_read(state_ptr, &mut dir, &mut info);
                    if rc == 0 {
                        break; // end of directory
                    }
                    if rc < 0 {
                        return Err(LfsError::from_lfs_error(rc));
                    }

                    // Extract the name from the C char array
                    let name_bytes = &info.name;
                    let name_len = name_bytes
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(name_bytes.len());
                    let name = std::str::from_utf8(&std::slice::from_raw_parts(
                        name_bytes.as_ptr() as *const u8,
                        name_len,
                    ))
                    .unwrap_or("<invalid utf8>")
                    .to_string();

                    // Skip "." and ".."
                    if name == "." || name == ".." {
                        continue;
                    }

                    let is_dir = info.type_ as u32 == lfs::lfs_type_LFS_TYPE_DIR;

                    entries.push(DirEntry {
                        name,
                        size: info.size as usize,
                        is_dir,
                    });
                }
                Ok(entries)
            })();

            let close_result = check(lfs::lfs_dir_close(state_ptr, &mut dir));
            let entries = result?;
            close_result?;
            Ok(entries)
        }
    }

    /// Remove a file or empty directory.
    pub fn remove(&self, path: &str) -> Result<(), LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            check(lfs::lfs_remove(state_ptr, cpath.as_ptr()))
        }
    }

    /// Rename or move a file or directory.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), LfsError> {
        let cfrom = to_cpath(from)?;
        let cto = to_cpath(to)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            check(lfs::lfs_rename(state_ptr, cfrom.as_ptr(), cto.as_ptr()))
        }
    }

    /// Get metadata (type and size) for a path.
    pub fn stat(&self, path: &str) -> Result<DirEntry, LfsError> {
        let cpath = to_cpath(path)?;
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            let mut info: lfs::lfs_info = std::mem::zeroed();
            check(lfs::lfs_stat(state_ptr, cpath.as_ptr(), &mut info))?;

            let name_bytes = &info.name;
            let name_len = name_bytes
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(name_bytes.len());
            let name = std::str::from_utf8(std::slice::from_raw_parts(
                name_bytes.as_ptr() as *const u8,
                name_len,
            ))
            .unwrap_or("<invalid utf8>")
            .to_string();

            let is_dir = info.type_ as u32 == lfs::lfs_type_LFS_TYPE_DIR;

            Ok(DirEntry {
                name,
                size: info.size as usize,
                is_dir,
            })
        }
    }

    /// Check whether a path exists.
    pub fn exists(&self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    /// Get the number of blocks in use on the filesystem.
    /// This is a lower bound — shared COW structures may inflate the count.
    pub fn used_blocks(&self) -> Result<usize, LfsError> {
        unsafe {
            let state_ptr = self.state as *const lfs::lfs_t as *mut lfs::lfs_t;
            let rc = lfs::lfs_fs_size(state_ptr);
            check_positive(rc)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ImageConfig {
        ImageConfig::from(4096, 16, 256, 256)
    }

    #[test]
    fn format_and_mount() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        assert!(image.is_mountable());
    }

    #[test]
    fn unformatted_not_mountable() {
        let mut image = LfsImage::new(test_config()).unwrap();
        assert!(!image.is_mountable());
    }

    #[test]
    fn write_and_read_file() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/hello.txt", b"Hello, LittleFS!")?;
                let data = fs.read_file("/hello.txt")?;
                assert_eq!(data, b"Hello, LittleFS!");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn create_directories() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.create_dir_all("/a/b/c")?;
                assert!(fs.exists("/a"));
                assert!(fs.exists("/a/b"));
                assert!(fs.exists("/a/b/c"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn list_directory() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.create_dir("/mydir")?;
                fs.write_file("/mydir/a.txt", b"aaa")?;
                fs.write_file("/mydir/b.txt", b"bbbbb")?;

                let entries = fs.read_dir("/mydir")?;
                assert_eq!(entries.len(), 2);

                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                assert!(names.contains(&"a.txt"));
                assert!(names.contains(&"b.txt"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn persistence_across_mounts() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        // Write in first mount
        image
            .mount_and_then(|fs| {
                fs.write_file("/persistent.txt", b"I survive unmount")?;
                Ok(())
            })
            .unwrap();

        // Read in second mount
        image
            .mount_and_then(|fs| {
                let data = fs.read_file("/persistent.txt")?;
                assert_eq!(data, b"I survive unmount");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn roundtrip_image_data() {
        let config = test_config();
        let mut image = LfsImage::new(config.clone()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/test.bin", &[42u8; 1000])?;
                Ok(())
            })
            .unwrap();

        // Serialize and deserialize
        let raw = image.into_data();
        let mut image2 = LfsImage::from_data(config, raw).unwrap();

        image2
            .mount_and_then(|fs| {
                let data = fs.read_file("/test.bin")?;
                assert_eq!(data.len(), 1000);
                assert!(data.iter().all(|&b| b == 42));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn small_block_size() {
        let config = ImageConfig::from(128, 64, 16, 16);
        let mut image = LfsImage::new(config).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/small.txt", b"works with 128-byte blocks")?;
                let data = fs.read_file("/small.txt")?;
                assert_eq!(data, b"works with 128-byte blocks");
                Ok(())
            })
            .unwrap();
    }
}
