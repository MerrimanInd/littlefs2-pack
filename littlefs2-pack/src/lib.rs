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
};

pub mod config;
pub mod littlefs;
pub mod pack;

/// Generate a LittleFS image from the LittleFS.toml
pub fn generate(littlefs_config: &Path) {
    let image_name = String::from("filesystem");
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let img_file_path = format!("{}/{}.bin", out_dir, image_name);
    // let rust_file_path = format!("{}/{}.rs", out_dir, image_name);

    let config = Config::from_file(littlefs_config).unwrap();

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
            "filesystem.bin",
            Some((&packed.dirs, &packed.files)),
        )
        .unwrap();

    let binary = image.into_data();

    std::fs::write(img_file_path, &binary).unwrap();
}
