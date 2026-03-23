# LittleFS Rust Tooling

This project provides a toolbox for deploying [LittleFS filesystems](https://github.com/littlefs-project/littlefs) to Rust embedded projects with as much determinism and robustness as possible. It can pack a directory into a LittleFS image, unpack an image back into its directory structure, and inspect the contents of an image. It can also synchronize a local directory to a microcontroller by building the image and sending it to the micro (if the files have changed) as part of the flashing process.

These tools build off of the Rust LittleFS bindings built by the Trussed Dev team. Their main [`littlefs2`](https://github.com/trussed-dev/littlefs2) is used by the firmware projects themselves to access LittleFS images. These tools use [`littlefs2-sys`](https://github.com/trussed-dev/littlefs2-sys), the low-level C bindings, for the actual packing and unpacking.

## LittleFS Config Files

LittleFS images have quite a few configuration options that must match between packing the image and then accessing it on the device. A single source of truth is necessary to maintain this alignment. Factoring in the myriad other configuration options it was logical to store them in a configuration file. This is a TOML file, generally stored at the root of your project repository and named `littlefs.toml`.

The configuration file has three main sections for the image to be created, the directory that will be packed into it, and the process for flashing it. There's a full example at `littlefs2-pack/littlefs.toml` with every option and comments explaining them but a minimal example might look like:

```toml
[image]
# This is the name of the generated files. It defaults to "filesystem" but can be used to differentiate multiple images
name = "filesystem"
block_size = 4096
page_size = 256
read_size = 16
write_size = 512
block_count = 3096
# Note that the total size of the image can also be defined with image_size
# But this is mutually exclusive to block_count and most be a multiple of block_size
# image_size = 15_998_976
cache_size = 256
lookahead_size = 8

[directory]
root = "./image_directory"
depth = -1  # Unlimited recursion
ignore_hidden = true  # Ignore hidden dotfiles
gitignore = true  # Respect the gitignore files found in the directory
repo_gitignore = true  # Respect the repo level gitignore
glob_ignores = ["*.bkup", "build"]  # Global ignore patterns
glob_includes = []  # Global includes that supercede all ignore patterns

[flash.firmware]
command = "espflash flash --chip esp32s3 {path}"

[flash.filesystem]
command = "espflash write-bin {address} {path}"
address = "0x200000"
```

Some of these are optional and have default values. Most should be self-explanatory from the comments.

## CLI Tool

The easiest way to interact with LittleFS images is through the CLI tool. You can install it with Cargo:

```bash
cargo install littlefs2-tool
```

This installs a binary called `littlefs` which has options for packing, unpacking, and inspecting LittleFS images. This is the only part of the project that can be used for non-Rust projects!

```bash
littlefs
Create, unpack, and inspect LittleFSv2 filesystem images

Usage: littlefs [OPTIONS] <COMMAND>

Commands:
  pack    Pack a directory into a LittleFS2 image
  unpack  Unpack a LittleFS2 image into a directory
  list    List files in a LittleFS2 image
  info    Print info about a LittleFS2 image (block count, used space, etc.)
  flash   Run the flash commands from a TOML config file
  help    Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>  Path to a littlefs.toml configuration file
  -h, --help             Print help
  -V, --version          Print version
```

All of the commands can take a path to a config file as an input or have a config file defined with the constituent flags (`--block-count`, `--block-size`, etc). The flash command is intended for a different use case, discussed in the Flash Runner section.

## `build.rs` Integration

The first and best place to use `littlefs2-pack` is in the `build.rs` file. This file is compiled and run before the rest of the Rust crate is compiled, making it an ideal time to build the image. This is a minimal example `build.rs`:

```rust
use std::path::Path;

use littlefs2_pack;

fn main() {
    littlefs2_pack::pack_and_generate_config(&Path::new("./littlefs.toml"));
}

```

The `pack_and_generate_config()` function takes in a path to a LittleFS config file and then on build it performs the following steps:
- Walks the directory, following the ignore and include rules, and lists the files and directories to be included
- Creates and format a LittleFS image in the OUT_DIR with the settings in the config file
- Adds the discovered files and directories to the image
- Copies the image up into the `target/<profile>` directory for easier access at runtime
- Generates a Rust file with some constants and modules to be used by the project

This runs every time `cargo build` is called, keeping the image up-to-date.

### ESP-IDF Partitions File Generation

Espressif ESP projects can use a `partitions.csv` file to define a set of partitions for a project, often including one or more used for a LittleFS image.

```csv
# ESP32-S3 FeatherS3 partition table — 16 MB flash
# Name,      Type, SubType,  Offset,     Size
nvs,         data, nvs,      0x9000,     0x6000
phy_init,    data, phy,      0xf000,     0x1000
factory,     app,  factory,  0x10000,    0x1F0000
littlefs,    data, fat,      0x200000,   0xE00000
```

Since the address at which this partition lives is used in the flashing process and sometimes the firmware itself it would be useful to also be able to treat this as a single source of truth. To enable this, `littlefs2-pack` also includes a function for generating a Rust file from the partitions file:

```rust
littlefs2_pack::generate_esp_partitions_config(&Path::new("./partitions.csv"), "littlefs");
```

which will generate something like:

```rust
// Auto-generated by littlefs2-pack — do not edit.

pub const PARTITION_NAME: &str = "littlefs";
pub const PARTITION_OFFSET: u32 = 0x200000;
pub const PARTITION_SIZE: u32 = 0xE00000;
```

## Rust Config Module

The Rust file that `build.rs` generates can be used by the firmware project for anything related to the LittleFS image. From a typical LittleFS config file the emitted Rust function might look like:

```rust
// Auto-generated by littlefs2-pack — do not edit.
use generic_array::typenum;

pub const BLOCK_SIZE: usize = 1024;
pub const BLOCK_COUNT: usize = 3096;
pub const READ_SIZE: usize = 16;
pub const WRITE_SIZE: usize = 512;
pub const CACHE_SIZE: usize = 512;
pub const LOOKAHEAD_SIZE: usize = 392;
pub const TOTAL_SIZE: usize = BLOCK_SIZE * BLOCK_COUNT;

/// Typenum alias for `littlefs2::driver::Storage::CACHE_SIZE`.
pub type CacheSize = typenum::U512;
/// Typenum alias for `littlefs2::driver::Storage::LOOKAHEAD_SIZE`.
/// Note: the littlefs2 crate measures lookahead in units of 8 bytes,
/// so this is `lookahead_size / 8`.
pub type LookaheadSize = typenum::U49;

/// The packed LittleFS image, embedded at compile time.
pub static IMAGE: &[u8] = include_bytes!("filesystem.bin");

pub mod paths {
    pub mod directory_a {
        pub const DIR: &str = "/directory_a";
        pub const FILE_MD: &str = "/directory_a/file.md";
    }
    pub mod directory_b {
        pub const DIR: &str = "/directory_b";
        pub const ANOTHER_FILE_TXT: &str = "/directory_b/another_file.txt";
    }
}
```

This would then be imported by the project file:

```rust
#[allow(unused)]
mod lfs_config {
    include!(concat!(env!("OUT_DIR"), "/littlefs_config.rs"));
}
```

### LittleFS Image Parameters

All of the parameters relating to the actual image are generated as constants, including the `CacheSize` and `LookaheadSize` in their appropriate `typenum` variants. These can be directly used by the `littlefs2` Rust crate to generate the storage struct. For example:

```rust
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
```

This struct correctly accesses a LittleFS image with every single parameter defined by the `lfs_config` module.

### Image Import

The `pub static IMAGE` line can be used as a convenience method for importing the bytes of an image into PSRAM:

```rust
// Allocate in PSRAM and copy the image in
let mut storage_buf = vec![0u8; lfs_config::TOTAL_SIZE];
storage_buf[..lfs_config::IMAGE.len()].copy_from_slice(lfs_config::IMAGE);
```

### Paths

The final part of the Rust config file is the entire directory tree of the packed image. This can be quite a few lines, the example above only featured a few files and directories. The benefit of this module is that specific paths in the image can be referred to with compile time checking. So if a file is moved, renamed, or not packed due to an ignore rule there will be a compiler error at build time.

For example, a top-level `index.html` file could be referenced with `lfs_config::paths::INDEX_HTML`. while a deeper path could be `lfs_config::paths::img::LOGO_PNG`. The dot separator between file name and suffix is replaced with an underscore and capitalized, as is convention with Rust constants. A directory itself can be referenced with the DIR constant: `lfs_config::paths::css::DIR`.

## Flash Runner

The final step of the process is to deploy both the firmware and the filesystem binaries to the embedded device. In most workflows and with most flashing tools, writing the two binaries are different commands. But in Rust projects we would really like to run the entirety of the project with `cargo run`.

To bring this functionality to a LittleFS project the CLI tool includes a flash command, `littlefs flash`. This can be used as the runner command in a Cargo project, specifically in the `.cargo/config.toml`

```rust
[target.<target-triple>]
runner = "littlefs flash --config ./littlefs.toml"
```

Cargo appends the path to the built image to the end of the runner argument when `cargo run` is called. This tool takes that binary, takes the LittleFS configuration, and then runs both commands. First it writes the filesystem binary and then the firmware binary. The tool gets the two runner commands from the configuration file: the `[flash]` section includes the command to be run for the firmware and filesystem writes as well as the address to which to write the filesystem binary. This makes the LittleFS tool agnostic to the specific file flash tools used.

In the case of an ESP project, instead of the actual path address the LittleFS config file can also reference the `partitions.csv` file:

```toml
[flash.filesystem]
command = "espflash write-bin {address} {path}"
# The ESP partitition table and name to read the address from
partition_table = "./partitions.csv"
partition_name = "littlefs"
```
