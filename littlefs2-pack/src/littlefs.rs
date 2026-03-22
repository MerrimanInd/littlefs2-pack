use std::collections::BTreeMap;
use std::ffi::{CString, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::string::String;

use crate::config::{DirectoryConfig, ImageConfig};
use crate::walk::{PathSet, walk_directory, walk_directory_simple};
use littlefs2_sys as lfs;
use std::fmt::Write as _;

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
    Io(#[from] std::io::Error),

    #[error("Path contains interior NUL byte")]
    NulPath,

    #[error("Error walking the directory: {0}")]
    Walk(#[from] crate::walk::WalkError),
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
// Manifest
// ---------------------------------------------------------------------------

pub struct ManifestEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: usize,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
/// Validate that the config values are acceptable to the LittleFS C library.
fn validate_for_lfs(config: &ImageConfig) -> Result<(), LfsError> {
    if config.block_size < 128 {
        return Err(LfsError::InvalidConfig("block_size must be >= 128".into()));
    }
    if config.block_count == 0 {
        return Err(LfsError::InvalidConfig("block_count must be > 0".into()));
    }
    if config.read_size == 0 || config.write_size == 0 {
        return Err(LfsError::InvalidConfig(
            "read_size and write_size must be > 0".into(),
        ));
    }
    if config.block_size % config.read_size != 0 {
        return Err(LfsError::InvalidConfig(
            "block_size must be a multiple of read_size".into(),
        ));
    }
    if config.block_size % config.write_size != 0 {
        return Err(LfsError::InvalidConfig(
            "block_size must be a multiple of write_size".into(),
        ));
    }
    Ok(())
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

        Ok(LfsImage {
            data: vec![0xFF; total],
            read_cache: vec![0u8; config.cache_size],
            write_cache: vec![0u8; config.cache_size],
            lookahead_buf: vec![0u8; config.lookahead_size],
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

        Ok(LfsImage {
            data,
            read_cache: vec![0u8; config.cache_size],
            write_cache: vec![0u8; config.cache_size],
            lookahead_buf: vec![0u8; config.lookahead_size],
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

    pub fn manifest(&mut self) -> Result<Vec<ManifestEntry>, LfsError> {
        self.mount_and_then(|fs| {
            let mut entries = Vec::new();
            fs.walk_recursive("/", &mut entries)?;
            Ok(entries)
        })
    }

    pub fn pack_from_config(&mut self, dir_config: DirectoryConfig) -> Result<(), LfsError> {
        let to_pack = walk_directory(&dir_config)?;

        self.pack_path_set(to_pack)?;

        Ok(())
    }

    pub fn pack_from_dir(&mut self, directory: &Path) -> Result<(), LfsError> {
        let to_pack = walk_directory_simple(directory)?;

        self.pack_path_set(to_pack)?;

        Ok(())
    }

    /// Internal function to pack a PathSet
    fn pack_path_set(&mut self, to_pack: PathSet) -> Result<(), LfsError> {
        self.mount_and_then(|fs| {
            for path in &to_pack.dirs {
                fs.create_dir_all(path)?;
            }
            for path in &to_pack.files {
                let data = std::fs::read(to_pack.host_path(path))?;
                fs.write_file(path, &data)?;
            }
            Ok(())
        })
    }

    // -- Internal: build the lfs_config struct pointing at our buffers ------

    /// Build an `lfs_config` that points back into `self` through a raw pointer.
    ///
    /// # Safety
    /// The returned config borrows `self` mutably through the `context` pointer.
    /// The caller must ensure `self` is not moved or dropped while the config
    /// is in use.
    ///
    /// This struct hardcodes specific values from name_max on, most notably the
    /// on `disk_version` param. This is because the `littlefs2` crate that reads
    /// the image also hardcodes these values (including staying on the disk version
    /// 2.0). Unfortunately this will just require hardcoded values in both crates
    /// and this will be checked when upgrading to newer versions of `littlefs2`.
    unsafe fn build_lfs_config(&mut self) -> lfs::lfs_config {
        lfs::lfs_config {
            context: self as *mut LfsImage as *mut c_void,
            read: Some(Self::lfs_read),
            prog: Some(Self::lfs_prog),
            erase: Some(Self::lfs_erase),
            sync: Some(Self::lfs_sync),
            read_size: self.config.read_size as u32,
            prog_size: self.config.write_size as u32,
            block_size: self.config.block_size as u32,
            block_count: self.config.block_count as u32,
            block_cycles: -1, // disable wear leveling for image creation
            cache_size: self.config.cache_size as u32,
            lookahead_size: self.config.lookahead_size as u32,
            read_buffer: self.read_cache.as_mut_ptr() as *mut c_void,
            prog_buffer: self.write_cache.as_mut_ptr() as *mut c_void,
            lookahead_buffer: self.lookahead_buf.as_mut_ptr() as *mut c_void,
            name_max: 255,
            file_max: 2147483647,
            attr_max: 1022,
            metadata_max: 0,
            inline_max: 0,
            compact_thresh: 0,
            disk_version: 0x00020000,
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

    /// Generate Rust constants for the image geometry and contents.
    ///
    /// Returns a string suitable for writing to a file and including
    /// via `include!()` in a firmware crate. Contains geometry constants,
    /// typenum type aliases, an `IMAGE` static that embeds the binary
    /// via `include_bytes!`, and a nested `pub mod paths { … }` tree
    /// mirroring the directory layout of the image.
    pub fn emit_rust(&mut self) -> Result<String, LfsError> {
        let lookahead_typenum_units = self.config.lookahead_size / 8;

        let mut content = format!(
            "// Auto-generated by littlefs2-pack — do not edit.\n\
             use generic_array::typenum;\n\
             \n\
             pub const BLOCK_SIZE: usize = {};\n\
             pub const BLOCK_COUNT: usize = {};\n\
             pub const READ_SIZE: usize = {};\n\
             pub const WRITE_SIZE: usize = {};\n\
             pub const CACHE_SIZE: usize = {};\n\
             pub const LOOKAHEAD_SIZE: usize = {};\n\
             pub const TOTAL_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;\n\
             \n\
             /// Typenum alias for `littlefs2::driver::Storage::CACHE_SIZE`.\n\
             pub type CacheSize = typenum::U{};\n\
             /// Typenum alias for `littlefs2::driver::Storage::LOOKAHEAD_SIZE`.\n\
             /// Note: the littlefs2 crate measures lookahead in units of 8 bytes,\n\
             /// so this is `lookahead_size / 8`.\n\
             pub type LookaheadSize = typenum::U{};\n\
             \n\
             /// The packed LittleFS image, embedded at compile time.\n\
             pub static IMAGE: &[u8] = include_bytes!(\"{}.bin\");\n",
            self.config.block_size,
            self.config.block_count,
            self.config.read_size,
            self.config.write_size,
            self.config.cache_size,
            self.config.lookahead_size,
            self.config.cache_size,
            lookahead_typenum_units,
            self.config.name,
        );

        let manifest = self.manifest()?;
        if !manifest.is_empty() {
            content.push('\n');
            emit_paths_mod(&mut content, &manifest);
        }

        Ok(content)
    }
}

// ---------------------------------------------------------------------------
// Path-module generation helpers
// ---------------------------------------------------------------------------

/// A tree node used to build the nested `pub mod paths { … }` structure.
#[derive(Default)]
struct PathNode {
    /// LFS path for the `DIR` constant (set for directory entries).
    dir_path: Option<String>,
    /// Files directly inside this directory: `(CONST_NAME, lfs_path)`.
    files: Vec<(String, String)>,
    /// Subdirectory modules: `mod_name → child node`.
    children: BTreeMap<String, PathNode>,
}

/// Convert a file name (e.g. `network.json`) to `UPPER_SNAKE_CASE` (`NETWORK_JSON`).
fn to_const_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            // Collapse consecutive separators into a single underscore.
            if !out.ends_with('_') {
                out.push('_');
            }
        }
    }
    // Trim leading/trailing underscores.
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        return "_UNNAMED".to_string();
    }
    // Prefix with `_` if the name starts with a digit (not a valid Rust ident start).
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Convert a directory name (e.g. `my-config`) to a valid Rust module name (`my_config`).
fn to_mod_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        return "_unnamed".to_string();
    }
    if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{trimmed}")
    } else {
        trimmed
    }
}

/// Build a [`PathNode`] tree from a manifest.
fn build_path_tree(manifest: &[ManifestEntry]) -> PathNode {
    let mut root = PathNode::default();

    for entry in manifest {
        let segments = path_segments(&entry.path);
        if entry.is_dir {
            let node = walk_to_node(&mut root, &segments);
            node.dir_path = Some(entry.path.clone());
        } else {
            if segments.is_empty() {
                continue;
            }
            let (file_name, parent_segs) = segments.split_last().unwrap();
            let node = walk_to_node(&mut root, parent_segs);
            node.files
                .push((to_const_name(file_name), entry.path.clone()));
        }
    }

    root
}

/// Split an LFS path like `/config/network.json` into `["config", "network.json"]`.
fn path_segments(lfs_path: &str) -> Vec<&str> {
    lfs_path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Walk (and lazily create) intermediate nodes to reach the node for `segments`.
fn walk_to_node<'a>(root: &'a mut PathNode, segments: &[&str]) -> &'a mut PathNode {
    let mut current = root;
    for &seg in segments {
        let mod_name = to_mod_name(seg);
        current = current.children.entry(mod_name).or_default();
    }
    current
}

/// Recursively write the `pub mod …` tree into `out`.
fn write_node(out: &mut String, node: &PathNode, indent: usize) {
    let pad = " ".repeat(indent);

    if let Some(ref dir_path) = node.dir_path {
        let _ = writeln!(out, "{pad}pub const DIR: &str = \"{dir_path}\";");
    }

    for (const_name, lfs_path) in &node.files {
        let _ = writeln!(out, "{pad}pub const {const_name}: &str = \"{lfs_path}\";");
    }

    for (mod_name, child) in &node.children {
        let _ = writeln!(out, "{pad}pub mod {mod_name} {{");
        write_node(out, child, indent + 4);
        let _ = writeln!(out, "{pad}}}");
    }
}

/// Append a `pub mod paths { … }` block to `out`.
fn emit_paths_mod(out: &mut String, manifest: &[ManifestEntry]) {
    let tree = build_path_tree(manifest);

    // If neither dirs nor files produced any content, skip emitting.
    if tree.children.is_empty() && tree.files.is_empty() {
        return;
    }

    out.push_str("pub mod paths {\n");

    // Root-level files (files sitting directly under `/`).
    for (const_name, lfs_path) in &tree.files {
        let _ = writeln!(out, "    pub const {const_name}: &str = \"{lfs_path}\";");
    }

    for (mod_name, child) in &tree.children {
        let _ = writeln!(out, "    pub mod {mod_name} {{");
        write_node(out, child, 8);
        let _ = writeln!(out, "    }}");
    }

    out.push_str("}\n");
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
                    Err(LfsError::Lfs(
                        format!("short write: {} of {} bytes", written, data.len()),
                        0, // todo(xmc) - this isn't a real error code
                    ))
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

    /// Recursively walk the filesystem and return a manifest of the contents
    fn walk_recursive(&self, path: &str, entries: &mut Vec<ManifestEntry>) -> Result<(), LfsError> {
        let dir_contents = self.read_dir(path)?;

        for entry in &dir_contents {
            let full_path = if path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", path, entry.name)
            };

            entries.push(ManifestEntry {
                path: full_path.clone(),
                is_dir: entry.is_dir,
                size: entry.size,
            });

            if entry.is_dir {
                self.walk_recursive(&full_path, entries)?;
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_IMAGE_NAME;

    fn test_config() -> ImageConfig {
        ImageConfig {
            block_size: 4096,
            block_count: 16,
            read_size: 256,
            write_size: 256,
            block_cycles: -1,
            cache_size: 256,
            lookahead_size: 8,
            name: DEFAULT_IMAGE_NAME.into(),
        }
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
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/test.bin", &[42u8; 1000])?;
                Ok(())
            })
            .unwrap();

        // Serialize and deserialize
        let raw = image.into_data();
        let mut image2 = LfsImage::from_data(test_config(), raw).unwrap();

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
        let config = ImageConfig {
            block_size: 128,
            block_count: 64,
            read_size: 16,
            write_size: 16,
            block_cycles: -1,
            cache_size: 16,
            lookahead_size: 1,
            name: DEFAULT_IMAGE_NAME.into(),
        };
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

    // -------------------------------------------------------------------------
    // manifest: reading image contents
    // -------------------------------------------------------------------------

    #[test]
    fn manifest_empty_image() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        let entries = image.manifest().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn manifest_flat_files() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/a.txt", b"aaa")?;
                fs.write_file("/b.txt", b"bbbbb")?;
                Ok(())
            })
            .unwrap();

        let entries = image.manifest().unwrap();
        assert_eq!(entries.len(), 2);

        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/a.txt"));
        assert!(paths.contains(&"/b.txt"));
        assert!(entries.iter().all(|e| !e.is_dir));
    }

    #[test]
    fn manifest_nested_structure() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.create_dir_all("/config/network")?;
                fs.write_file("/config/network/wifi.json", b"{}")?;
                fs.write_file("/config/app.toml", b"[app]")?;
                fs.write_file("/index.html", b"<html>")?;
                Ok(())
            })
            .unwrap();

        let entries = image.manifest().unwrap();

        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"/config"));
        assert!(paths.contains(&"/config/network"));
        assert!(paths.contains(&"/config/network/wifi.json"));
        assert!(paths.contains(&"/config/app.toml"));
        assert!(paths.contains(&"/index.html"));

        // Check dir/file flags
        let config_entry = entries.iter().find(|e| e.path == "/config").unwrap();
        assert!(config_entry.is_dir);

        let index_entry = entries.iter().find(|e| e.path == "/index.html").unwrap();
        assert!(!index_entry.is_dir);
    }

    #[test]
    fn manifest_reports_file_sizes() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/empty.txt", b"")?;
                fs.write_file("/small.txt", b"hello")?;
                fs.write_file("/larger.bin", &[0xAB; 500])?;
                Ok(())
            })
            .unwrap();

        let entries = image.manifest().unwrap();

        let empty = entries.iter().find(|e| e.path == "/empty.txt").unwrap();
        assert_eq!(empty.size, 0);

        let small = entries.iter().find(|e| e.path == "/small.txt").unwrap();
        assert_eq!(small.size, 5);

        let larger = entries.iter().find(|e| e.path == "/larger.bin").unwrap();
        assert_eq!(larger.size, 500);
    }

    #[test]
    fn manifest_dirs_have_zero_size() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.create_dir("/mydir")?;
                fs.write_file("/mydir/file.txt", b"content")?;
                Ok(())
            })
            .unwrap();

        let entries = image.manifest().unwrap();
        let dir = entries.iter().find(|e| e.path == "/mydir").unwrap();
        assert!(dir.is_dir);
        assert_eq!(dir.size, 0);
    }

    #[test]
    fn manifest_does_not_include_root() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.write_file("/test.txt", b"data")?;
                Ok(())
            })
            .unwrap();

        let entries = image.manifest().unwrap();
        assert!(!entries.iter().any(|e| e.path == "/"));
    }

    // -------------------------------------------------------------------------
    // pack_from_dir: packing a host directory into an image
    // -------------------------------------------------------------------------

    fn create_test_directory(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("css")).unwrap();
        std::fs::create_dir_all(root.join("js")).unwrap();
        std::fs::write(root.join("index.html"), "<html>hello</html>").unwrap();
        std::fs::write(root.join("css/style.css"), "body {}").unwrap();
        std::fs::write(root.join("js/app.js"), "console.log('hi')").unwrap();
    }

    #[test]
    fn pack_from_dir_creates_structure() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        image.pack_from_dir(dir.path()).unwrap();

        image
            .mount_and_then(|fs| {
                assert!(fs.exists("/index.html"));
                assert!(fs.exists("/css/style.css"));
                assert!(fs.exists("/js/app.js"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_from_dir_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        image.pack_from_dir(dir.path()).unwrap();

        image
            .mount_and_then(|fs| {
                let data = fs.read_file("/test.txt")?;
                assert_eq!(data, b"hello world");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn pack_from_dir_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let pack_once = || {
            let mut image = LfsImage::new(test_config()).unwrap();
            image.format().unwrap();
            image.pack_from_dir(dir.path()).unwrap();
            image.into_data()
        };

        assert_eq!(pack_once(), pack_once());
    }

    #[test]
    fn pack_from_dir_empty_directory() {
        let dir = tempfile::tempdir().unwrap();

        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        image.pack_from_dir(dir.path()).unwrap();

        let entries = image.manifest().unwrap();
        assert!(entries.is_empty());
    }

    // -------------------------------------------------------------------------
    // manifest after pack: end-to-end
    // -------------------------------------------------------------------------

    #[test]
    fn manifest_after_pack_reflects_contents() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        image.pack_from_dir(dir.path()).unwrap();

        let entries = image.manifest().unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();

        assert!(paths.contains(&"/index.html"));
        assert!(paths.contains(&"/css"));
        assert!(paths.contains(&"/css/style.css"));
        assert!(paths.contains(&"/js"));
        assert!(paths.contains(&"/js/app.js"));

        // Verify sizes match what was written
        let html = entries.iter().find(|e| e.path == "/index.html").unwrap();
        assert_eq!(html.size, "<html>hello</html>".len());
        assert!(!html.is_dir);

        let css_dir = entries.iter().find(|e| e.path == "/css").unwrap();
        assert!(css_dir.is_dir);
    }

    #[test]
    fn manifest_after_manual_writes_matches_pack() {
        // Build the same image two ways and verify manifests match
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/file.txt"), "content").unwrap();

        // Way 1: pack_from_dir
        let mut packed = LfsImage::new(test_config()).unwrap();
        packed.format().unwrap();
        packed.pack_from_dir(dir.path()).unwrap();

        // Way 2: manual writes
        let mut manual = LfsImage::new(test_config()).unwrap();
        manual.format().unwrap();
        manual
            .mount_and_then(|fs| {
                fs.create_dir("/data")?;
                fs.write_file("/data/file.txt", b"content")?;
                Ok(())
            })
            .unwrap();

        let packed_manifest = packed.manifest().unwrap();
        let manual_manifest = manual.manifest().unwrap();

        assert_eq!(packed_manifest.len(), manual_manifest.len());
        for (p, m) in packed_manifest.iter().zip(manual_manifest.iter()) {
            assert_eq!(p.path, m.path);
            assert_eq!(p.is_dir, m.is_dir);
            assert_eq!(p.size, m.size);
        }
    }

    // -------------------------------------------------------------------------
    // to_const_name / to_mod_name helpers
    // -------------------------------------------------------------------------

    #[test]
    fn const_name_simple() {
        assert_eq!(to_const_name("style.css"), "STYLE_CSS");
    }

    #[test]
    fn const_name_multiple_dots() {
        assert_eq!(to_const_name("app.min.js"), "APP_MIN_JS");
    }

    #[test]
    fn const_name_hyphens() {
        assert_eq!(to_const_name("my-file.txt"), "MY_FILE_TXT");
    }

    #[test]
    fn const_name_consecutive_separators() {
        assert_eq!(to_const_name("a--b..c"), "A_B_C");
    }

    #[test]
    fn const_name_leading_digit() {
        assert_eq!(to_const_name("404.html"), "_404_HTML");
    }

    #[test]
    fn const_name_all_separators() {
        assert_eq!(to_const_name("---"), "_UNNAMED");
    }

    #[test]
    fn mod_name_simple() {
        assert_eq!(to_mod_name("config"), "config");
    }

    #[test]
    fn mod_name_hyphens() {
        assert_eq!(to_mod_name("my-config"), "my_config");
    }

    #[test]
    fn mod_name_leading_digit() {
        assert_eq!(to_mod_name("2024"), "_2024");
    }

    // -------------------------------------------------------------------------
    // emit_paths_mod
    // -------------------------------------------------------------------------

    #[test]
    fn emit_paths_mod_empty_manifest() {
        let mut out = String::new();
        emit_paths_mod(&mut out, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn emit_paths_mod_root_file_only() {
        let manifest = vec![ManifestEntry {
            path: "/index.html".to_string(),
            is_dir: false,
            size: 100,
        }];

        let mut out = String::new();
        emit_paths_mod(&mut out, &manifest);

        assert!(out.contains("pub mod paths {"));
        assert!(out.contains(r#"pub const INDEX_HTML: &str = "/index.html";"#));
    }

    #[test]
    fn emit_paths_mod_directory_with_files() {
        let manifest = vec![
            ManifestEntry {
                path: "/config".to_string(),
                is_dir: true,
                size: 0,
            },
            ManifestEntry {
                path: "/config/network.json".to_string(),
                is_dir: false,
                size: 50,
            },
        ];

        let mut out = String::new();
        emit_paths_mod(&mut out, &manifest);

        assert!(out.contains("pub mod config {"));
        assert!(out.contains(r#"pub const DIR: &str = "/config";"#));
        assert!(out.contains(r#"pub const NETWORK_JSON: &str = "/config/network.json";"#));
    }

    #[test]
    fn emit_paths_mod_nested_directories() {
        let manifest = vec![
            ManifestEntry {
                path: "/a".to_string(),
                is_dir: true,
                size: 0,
            },
            ManifestEntry {
                path: "/a/b".to_string(),
                is_dir: true,
                size: 0,
            },
            ManifestEntry {
                path: "/a/b/deep.txt".to_string(),
                is_dir: false,
                size: 10,
            },
        ];

        let mut out = String::new();
        emit_paths_mod(&mut out, &manifest);

        assert!(out.contains("pub mod a {"));
        assert!(out.contains("pub mod b {"));
        assert!(out.contains(r#"pub const DEEP_TXT: &str = "/a/b/deep.txt";"#));
    }

    // -------------------------------------------------------------------------
    // emit_rust: full code generation
    // -------------------------------------------------------------------------

    #[test]
    fn emit_rust_contains_config_constants() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        let output = image.emit_rust().unwrap();

        assert!(output.contains("pub const BLOCK_SIZE: usize = 4096;"));
        assert!(output.contains("pub const BLOCK_COUNT: usize = 16;"));
        assert!(output.contains("pub const READ_SIZE: usize = 256;"));
        assert!(output.contains("pub const WRITE_SIZE: usize = 256;"));
        assert!(output.contains("pub const CACHE_SIZE: usize = 256;"));
        assert!(output.contains("pub const LOOKAHEAD_SIZE: usize = 8;"));
        assert!(output.contains("pub const TOTAL_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;"));
    }

    #[test]
    fn emit_rust_contains_typenum_aliases() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        let output = image.emit_rust().unwrap();

        assert!(output.contains("pub type CacheSize = typenum::U256;"));
        // lookahead_size is 8, divided by 8 = 1
        assert!(output.contains("pub type LookaheadSize = typenum::U1;"));
    }

    #[test]
    fn emit_rust_contains_include_bytes() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        let output = image.emit_rust().unwrap();

        let expected = format!(r#"include_bytes!("{DEFAULT_IMAGE_NAME}.bin")"#);
        assert!(output.contains(&expected));
    }

    #[test]
    fn emit_rust_uses_provided_filename() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.config.name = String::from("custom_image");
        image.format().unwrap();

        let output = image.emit_rust().unwrap();

        assert!(output.contains(r#"include_bytes!("custom_image.bin")"#));
    }

    #[test]
    fn emit_rust_empty_image_no_paths_module() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        let output = image.emit_rust().unwrap();

        assert!(!output.contains("pub mod paths"));
    }

    #[test]
    fn emit_rust_with_files_generates_paths_module() {
        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();

        image
            .mount_and_then(|fs| {
                fs.create_dir("/config")?;
                fs.write_file("/config/network.json", b"{}")?;
                fs.write_file("/index.html", b"<html>")?;
                Ok(())
            })
            .unwrap();

        let output = image.emit_rust().unwrap();

        assert!(output.contains("pub mod paths {"));
        assert!(output.contains("pub mod config {"));
        assert!(output.contains(r#"pub const DIR: &str = "/config";"#));
        assert!(output.contains(r#"pub const NETWORK_JSON: &str = "/config/network.json";"#));
        assert!(output.contains(r#"pub const INDEX_HTML: &str = "/index.html";"#));
    }

    #[test]
    fn emit_rust_after_pack_generates_paths() {
        let dir = tempfile::tempdir().unwrap();
        create_test_directory(dir.path());

        let mut image = LfsImage::new(test_config()).unwrap();
        image.format().unwrap();
        image.pack_from_dir(dir.path()).unwrap();

        let output = image.emit_rust().unwrap();

        assert!(output.contains("pub mod paths {"));
        assert!(output.contains("pub mod css {"));
        assert!(output.contains("pub mod js {"));
        assert!(output.contains(r#"pub const INDEX_HTML: &str = "/index.html";"#));
        assert!(output.contains(r#"pub const STYLE_CSS: &str = "/css/style.css";"#));
        assert!(output.contains(r#"pub const APP_JS: &str = "/js/app.js";"#));
    }
}
