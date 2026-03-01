# littlefs2-pack
A Rust crate for building a file system into a LittleFS binary file to be flashed to an embedded device.

This crate wraps the [LittleFS C library](https://github.com/littlefs-project/littlefs) using the [`littlefs2-sys`](https://crates.io/crates/littlefs2-sys) crate. The [`littlefs2`](https://crates.io/crates/littlefs2) crate might have been an easier starting point but it doesn't currently allow setting the image configuration dynamically at runtime, such as the block size and count.

`littlefs2-pack` is tested for compatibility with the C++ [`mklittlefs` project](https://github.com/earlephilhower/mklittlefs). This is ensured with the `cross-compat.rs` test that packs with one tool then unpack with the other, in both directions. These tests are ran against the version of `mklittlefs` in the submodule and requires that tool to be built prior to running the tests.

## API

The crate can be called directly to create a LittleFS image from a target directory.

```rust,no_run
use littlefs2_pack::{LfsImage, LfsImageConfig};
let config = LfsImageConfig {
    block_size: 4096,
    block_count: 256,  // 1 MiB total
    read_size: 256,
    write_size: 256,
};
let mut image = LfsImage::new(config).unwrap();
image.format().unwrap();
image.mount_and_then(|fs| {
    fs.create_dir("/data")?;
    fs.write_file("/data/hello.txt", b"Hello from LittleFS!")?;
    Ok(())
}).unwrap();
let binary = image.into_data();
std::fs::write("filesystem.bin", &binary).unwrap();
```


## CLI

The command line interface allows for manually packing, unpacking, and inspecting LittleFS2 images.

```bash
Usage: littlefs2-pack <COMMAND>

Commands:
  pack    Pack a directory into a LittleFS2 image
  unpack  Unpack a LittleFS2 image into a directory
  list    List files in a LittleFS2 image
  info    Print info about a LittleFS2 image (block count, used space, etc.)
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```
