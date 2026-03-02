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

pub mod config;
pub mod littlefs;
pub mod pack;
