{
  description = "tofu-sdk-rs — clean-room Rust Terraform/OpenTofu provider SDK";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Up-to-date stable Rust toolchain with the components we develop against.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "tofu-sdk-rs";

          packages = with pkgs; [
            rustToolchain

            # gRPC / protobuf codegen (tonic + prost, Phase 2)
            protobuf # provides protoc

            # native build deps
            pkg-config
            openssl

            # tooling
            cargo-nextest # faster test runner
            cargo-watch # iterate on change
            cargo-expand # inspect derive/macro output (facet)
            cargo-llvm-cov # coverage (project targets 80%+)

            # interop / debugging against real Terraform/OpenTofu
            opentofu

            # JS toolchain for the @tofu-sdk/core Node binding (packages/tofu-sdk).
            # `napi build` shells out to cargo, so node/pnpm must share this shell
            # (where PROTOC is set for the terraform-tfplugin6 build).
            nodejs_22
            pnpm
          ];

          env = {
            # Let prost/tonic find the system protoc instead of trying to vendor one.
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            RUST_BACKTRACE = "1";
          };

          shellHook = ''
            echo "tofu-sdk-rs dev shell"
            echo "  rustc:  $(rustc --version)"
            echo "  cargo:  $(cargo --version)"
            echo "  protoc: $(protoc --version)"
            echo "  tofu:   $(tofu version 2>/dev/null | head -1)"
            echo "  node:   $(node --version)  pnpm: $(pnpm --version)"
          '';
        };
      }
    );
}
