{
  description = "littlefs2-tool";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        cargoTomlTool = fromTOML (builtins.readFile "${self}/littlefs2-tool/Cargo.toml");

        cDeps = with pkgs; [
          clang
          cmake
          gnumake
          gcc
        ];

        rustDeps = with pkgs; [
          pkg-config
        ];

        cLibs = with pkgs; [
          libclang.lib
        ];

        bindgenEnv = {
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu = toString [
            "-isystem"
            "${pkgs.glibc.dev}/include"
            "-isystem"
            "${pkgs.libclang.lib}/lib/clang/${pkgs.lib.versions.major pkgs.libclang.version}/include"
            "-target"
            "x86_64-unknown-linux-gnu"
          ];
        };

        cEnv = bindgenEnv // {
          AR = "ar";
          CC = "gcc";
        };
      in
      with pkgs;
      {
        devShells.default = mkShell (
          cEnv
          // {
            name = "littlefs2_tool";
            buildInputs = [
              rust-bin.stable.latest.default
            ]
            ++ cDeps
            ++ cLibs
            ++ rustDeps;

            LD_LIBRARY_PATH = "${lib.makeLibraryPath (cDeps ++ cLibs)}";
            MKLITTLEFS_CPP = "./mklittlefs/mklittlefs";
          }
        );

        devShells.esp = mkShell (
          bindgenEnv
          // {
            name = "lfs2_esp";
            buildInputs = [
              # Don't include rust-bin here — espup manages the toolchain
              rustup

              # ESP tooling
              esptool
              espflash
              esp-generate

              # Probe / debug
              probe-rs-tools
              openocd

              python3
            ]
            ++ cDeps
            ++ cLibs
            ++ rustDeps;

            LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

            LD_LIBRARY_PATH = lib.makeLibraryPath (
              cDeps ++ cLibs ++ [ stdenv.cc.cc.lib ] # provides libstdc++.so.6
            );

            # Don't set CC or AR globally — let the cc crate
            # find the xtensa cross-compiler from espup's PATH

            shellHook = ''
              if [ -f "$HOME/export-esp.sh" ]; then
                source "$HOME/export-esp.sh"
              fi

              # Set CC for the xtensa target only, leaving host CC alone
              export CC_xtensa_esp32s3_none_elf=xtensa-esp32s3-elf-gcc
              export AR_xtensa_esp32s3_none_elf=xtensa-esp32s3-elf-ar
            '';
          }
        );

        packages.default = rustPlatform.buildRustPackage (
          cEnv
          // {
            pname = cargoTomlTool.package.name;
            version = cargoTomlTool.package.version;
            src = self;
            meta.mainProgram = "littlefs";

            cargoLock = {
              lockFile = "${self}/Cargo.lock";
              outputHashes = { };
            };

            nativeBuildInputs = [ pkg-config ] ++ cDeps;
            buildInputs = cLibs;

            cargoBuildFlags = [
              "--package"
              "littlefs2-tool"
            ];
            cargoTestFlags = [
              "--package"
              "littlefs2-tool"
            ];

            postInstall = "";
          }
        );
      }
    );
}
