# littlefs2-tool

A command line interface tool for packing, unpacking, inspecting, and flashing LittleFS images to embedded devices.

There is also a library crate for integrating with the Rust build process called [`littlefs2-pack`](https://crates.io/crates/littlefs2-pack). See the [`littlefs-tooling-rs` GitHub project README](https://github.com/MerrimanInd/littlefs-tooling-rs/blob/main/README.md) for information on integrating the two in a build system.

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
