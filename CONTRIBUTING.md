# Contributing

This project is attempting to navigate this brave new post-AI open-source world. Having navigated some drive-by multi-thousand line AI-generated PRs in other projects, this project has a slightly higher barrier of entry. Specifically, we're closed to public PRs. I encourage anyone interested in getting involved to start discussions or open issues as we'd love to have additional contributors.

# Development

## With Nix

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

## Without Nix
If you can't use Nix (Windows can only run Nix in WSL) the tool can be built with the normal `cargo` commands, but your system will require `clang` and its C build toolchain to be installed.
