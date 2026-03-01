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

        cLibs = with pkgs; [
          libclang.lib
        ];

        cEnv = {
          AR = "ar";
          CC = "gcc";
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
      in
      with pkgs;
      {
        devShells.default = mkShell (
          cEnv
          // {
            buildInputs = [
              rust-bin.stable.latest.default
              pkg-config
            ]
            ++ cDeps
            ++ cLibs;

            LD_LIBRARY_PATH = "${lib.makeLibraryPath (cDeps ++ cLibs)}";
            MKLITTLEFS_CPP = "./mklittlefs/mklittlefs";
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
