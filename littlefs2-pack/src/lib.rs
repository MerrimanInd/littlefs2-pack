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
//! use littlefs2_pack::littlefs::LfsImage;
//! use littlefs2_pack::config::ImageConfig;
//!
//! let config = ImageConfig {
//!     block_size: 4096,
//!     block_count: 128,
//!     read_size: 256,
//!     write_size: 256,
//!     block_cycles: -1,
//!     cache_size: 256,
//!     lookahead_size: 8,
//! };
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

use std::path::Path;

use crate::{
    config::Config,
    littlefs::LfsImage,
    pack::{PackedPaths, pack_directory},
    partition_table::get_partition,
};

pub mod config;
pub mod littlefs;
pub mod pack;
pub mod partition_table;

/// Generate a LittleFS image and Rust configuration module from a
/// `littlefs.toml` file.
///
/// Reads the TOML configuration at `littlefs_config`, packs the
/// directory tree it references into a LittleFS binary image, and
/// writes two files into `$OUT_DIR`:
///
/// - **`filesystem.bin`** — the raw LittleFS image ready to be
///   flashed to the device.
/// - **`littlefs_config.rs`** — Rust constants for the image geometry
///   (`BLOCK_SIZE`, `BLOCK_COUNT`, `TOTAL_SIZE`, etc.), typenum
///   aliases, an `IMAGE` static that embeds the binary via
///   `include_bytes!`, and an optional `paths` module mirroring the
///   packed directory layout.
///
/// Also copies the built filesystem image up to the target/<profile> directory.
/// The `$OUT_DIR` is difficult to access during flash or runtime since it's
/// a hash encoded build directory. This step makes the image much easier to
/// find at flash time.
///
/// # Usage in `build.rs`
///
/// ```rust,no_run
/// littlefs2_pack::pack_and_generate_config(
///     std::path::Path::new("littlefs.toml"),
/// );
/// ```
///
/// Then in your firmware crate:
///
/// ```rust,ignore
/// mod littlefs {
///     include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
/// }
/// ```
///
/// # Panics
///
/// Panics if the `OUT_DIR` environment variable is not set (i.e. this
/// function is called outside of a Cargo build script), or if any step
/// of config parsing, image creation, packing, or file I/O fails. This
/// panic behavior is because a build should not proceed if this step
/// doesn't succeed.
pub fn pack_and_generate_config(littlefs_config: &Path) {
    // Load the config from the file
    let config = Config::from_file(littlefs_config).unwrap();

    let image_name = config.image.name.clone();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // OUT_DIR is like target/<triple>/<profile>/build/<crate>-<hash>/out
    // The issue with this directory is that it's only easily accessible
    // at compile time, it's hard to discern at run time. This script
    // copies the image up to the target profile directory after build
    // so the image can be found at flash time
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3) // out/ -> <crate>-<hash>/ -> build/ -> <profile>/
        .unwrap();
    let img_file_path = format!("{}/{}.bin", out_dir, image_name);
    let rust_file_path = format!("{}/{}.rs", out_dir, image_name);

    let image_config = config.image.clone();
    let mut image = LfsImage::new(config.image).unwrap();
    image.format().unwrap();

    let mut packed_paths: Option<PackedPaths> = None;
    image
        .mount_and_then(|fs| {
            let paths = pack_directory(fs, &config.directory).unwrap();
            packed_paths = Some(paths);
            Ok(())
        })
        .unwrap();

    let packed = packed_paths.unwrap();
    image_config
        .emit_rust(
            Path::new(&out_dir),
            &format!("{}.bin", image_name),
            Some((&packed.dirs, &packed.files)),
        )
        .unwrap();

    let binary = image.into_data();

    std::fs::write(&img_file_path, &binary).unwrap();

    // Copy the binary up to the profile directory
    std::fs::copy(&img_file_path, profile_dir.join("filesystem.bin")).unwrap();
}

/// Generate a Rust module with partition offset and size constants
/// from an ESP-IDF partition table CSV.
///
/// Reads the CSV at `partition_csv`, looks up the partition named
/// `partition_name`, and writes a `partition_config.rs` file into
/// `$OUT_DIR` containing:
///
/// ```text
/// pub const PARTITION_NAME: &str = "littlefs";
/// pub const PARTITION_OFFSET: u32 = 0x200000;
/// pub const PARTITION_SIZE: u32 = 0xE00000;
/// ```
///
/// # Usage in `build.rs`
///
/// ```rust,no_run
/// littlefs2_pack::generate_esp_partitions_config(
///     std::path::Path::new("partitions.csv"),
///     "littlefs",
/// );
/// ```
///
/// Then in your firmware crate:
///
/// ```rust,ignore
/// mod partition {
///     include!(concat!(env!("OUT_DIR"), "/partition_config.rs"));
/// }
/// ```
pub fn generate_esp_partitions_config(partition_csv: &Path, partition_name: &str) {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    let partition = get_partition(partition_csv, partition_name).unwrap();
    partition.emit_rust(Path::new(&out_dir)).unwrap();
}
