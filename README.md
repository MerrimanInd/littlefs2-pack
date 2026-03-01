# LittleFS Tooling

This project provides a Rust CLI for working with [the LittleFS file system](https://github.com/littlefs-project/littlefs). It can pack a directory into a LittleFS image, unpack an image back into its directory structure, and inspect the contents of an image. It can also synchronize a local directory to a microcontroller by building the image and sending it to the micro (if the files have changed) as part of the flashing process.

## Development

This project provides a Nix flake as the development shell. This shell contains the C libraries necessary to build `mklittlefs` (used for testing) and exports some useful environment variables. After [installing Nix](https://nixos.org/download/) the shell can be entered with the command:

```bash
nix develop
```

In the future, `nix build` and `nix run` commands will be added for building and/or running the tool.
