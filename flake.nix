{
  description = "sqlx-cel — transpiles a CEL expression into a SQL WHERE fragment for sqlx";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        # No `packages.default`. This is a library crate with no binary, and
        # `buildRustPackage` would want a committed Cargo.lock -- which a library
        # deliberately does not have. The dev shell is the whole point here.
        devShells.default = pkgs.mkShell {
          inherit (manifest) name;

          packages = with pkgs; [
            rust-toolchain
            pkg-config
            sqlite # the `sqlite3` CLI, for poking at tests/sqlite.rs
          ];

          # libsqlite3-sys builds a vendored SQLite, so the shell needs a C
          # compiler. mkShell's stdenv supplies one; this just makes `cc` the
          # name the build script expects on every platform.
          env.CC = "${pkgs.stdenv.cc}/bin/cc";

          shellHook = ''
            echo "${manifest.name} ${manifest.version} — $(cargo --version)"
            echo "  cargo test --features sqlite,mysql    # all drivers, incl. end-to-end SQLite"
            echo "  cargo clippy --all-targets --features sqlite,mysql"
          '';
        };
      }
    );
}
