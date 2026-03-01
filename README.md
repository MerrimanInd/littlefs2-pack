# LittleFS Tooling

This project provides a Rust CLI for working with [the LittleFS file system](https://github.com/littlefs-project/littlefs). It can pack a directory into a LittleFS image, unpack an image back into its directory structure, and inspect the contents of an image. It can also synchronize a local directory to a microcontroller by building the image and sending it to the micro (if the files have changed) as part of the flashing process.

## Development

### With Nix

This project provides a Nix flake as the development shell and build system. This shell contains the C libraries necessary to build `mklittlefs` (used for testing) and exports some useful environment variables. After [installing Nix](https://nixos.org/download/) the shell can be entered with the command:

```bash
nix develop
```

Likewise to build the `littlefs` tool binary use:

```bash
nix build
```
This will build the tool and save the binary to `result/bin/littlefs`. To build and run the tool in one command enter:

```bash
nix run
```

### Without Nix
If you can't use Nix (Windows can only run Nix in WSL) the tool can be built with the normal `cargo` commands, but your system will require `clang` and its C build toolchain to be installed.
