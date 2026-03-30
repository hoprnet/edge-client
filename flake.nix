{
  description = "HOPR Edge client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-utils.url = "github:numtide/flake-utils";

    # HOPR Nix Library (provides reusable build functions)
    nix-lib.url = "github:hoprnet/nix-lib/v1.1.0";

    crane.url = "github:ipetkov/crane";

    pre-commit.url = "github:cachix/git-hooks.nix";

    treefmt-nix.url = "github:numtide/treefmt-nix";

    rust-overlay.url = "github:oxalica/rust-overlay";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    # Input dependency optimization
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    nix-lib.inputs.nixpkgs.follows = "nixpkgs";
    nix-lib.inputs.crane.follows = "crane";
    nix-lib.inputs.rust-overlay.follows = "rust-overlay";
    nix-lib.inputs.flake-utils.follows = "flake-utils";
    nix-lib.inputs.flake-parts.follows = "flake-parts";
    nix-lib.inputs.treefmt-nix.follows = "treefmt-nix";
    pre-commit.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      self,
      flake-parts,
      nixpkgs,
      nix-lib,
      rust-overlay,
      crane,
      advisory-db,
      treefmt-nix,
      pre-commit,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ treefmt-nix.flakeModule ];
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      perSystem =
        {
          config,
          self',
          inputs',
          lib,
          system,
          ...
        }:
        let
          localSystem = system;

          # Import nix-lib for this system
          nixLib = nix-lib.lib.${system};

          pkgs = import nixpkgs {
            inherit localSystem;
            overlays = [ (import rust-overlay) ];
          };

          # Create all Rust builders for cross-compilation using nix-lib
          builders = nixLib.mkRustBuilders {
            inherit localSystem;
            rustToolchainFile = ./rust-toolchain.toml;
          };

          # Import all edgli packages (uses nix-lib builders + mkRustPackage).
          # src, depsSrc, and rev are computed internally in edgli.nix.
          edgliPackages = import ./nix/edgli.nix {
            inherit
              builders
              nixLib
              self
              lib
              pkgs
              ;
          };

          systemTargets = {
            "x86_64-linux" = "x86_64-unknown-linux-musl";
            "aarch64-linux" = "aarch64-unknown-linux-musl";
            "x86_64-darwin" = "x86_64-apple-darwin";
            "aarch64-darwin" = "aarch64-apple-darwin";
          };

          targetForSystem = builtins.getAttr system systemTargets;

          # NB: we don't need to overlay our custom toolchain for the *entire*
          # pkgs (which would require rebuidling anything else which uses rust).
          # Instead, we just want to update the scope that crane will use by appending
          # our specific toolchain there.
          # cross = pkgs.pkgsCross.musl64;
          craneLib = (crane.mkLib pkgs).overrideToolchain (
            p:
            (p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
              targets = [ targetForSystem ];
            }
          );

          src = craneLib.cleanCargoSource ./.;

          # Common arguments can be set here to avoid repeating them later
          commonArgs = {
            inherit src;
            strictDeps = true;

            nativeBuildInputs = [
              pkgs.pkg-config
            ]
            ++ lib.optionals pkgs.stdenv.isLinux [
              pkgs.mold
            ];
            buildInputs = [
              pkgs.pkgsStatic.openssl
            ]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              # Additional darwin specific inputs can be set here
              pkgs.libiconv
            ];

            # Additional environment variables can be set directly
            # MY_CUSTOM_VAR = "some value";
          };

          # Build *just* the cargo dependencies (of the entire workspace),
          # so we can reuse all of that work (e.g. via cachix) when running in CI
          # It is *highly* recommended to use something like cargo-hakari to avoid
          # cache misses when building individual top-level-crates
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          pre-commit-check = pre-commit.lib.${system}.run {
            src = ./.;
            hooks = {
              # https://github.com/cachix/git-hooks.nix
              treefmt.enable = false;
              treefmt.package = config.treefmt.build.wrapper;
              check-executables-have-shebangs.enable = true;
              check-shebang-scripts-are-executable.enable = true;
              check-case-conflicts.enable = true;
              check-symlinks.enable = true;
              check-merge-conflicts.enable = true;
              check-added-large-files.enable = true;
              commitizen.enable = true;
            };
            tools = pkgs;
            excludes = [
            ];
          };

          treefmt = {
            projectRootFile = "LICENSE";

            settings.global.excludes = [
              "LICENSE"
              "LATEST"
              "target/*"
              "modules/*"
            ];

            programs.nixfmt.enable = pkgs.lib.meta.availableOn pkgs.stdenv.buildPlatform pkgs.nixfmt.compiler;
            programs.deno.enable = true;
            settings.formatter.deno.excludes = [
              "*.toml"
              "*.yml"
              "*.yaml"
            ];
            programs.rustfmt.enable = true;
            programs.shellcheck.enable = true;
            programs.shfmt = {
              enable = true;
              indent_size = 4;
            };
            programs.taplo.enable = true; # TOML formatter
            programs.yamlfmt.enable = true;
            # trying setting from https://github.com/google/yamlfmt/blob/main/docs/config-file.md
            settings.formatter.yamlfmt.settings = {
              formatter.type = "basic";
              formatter.max_line_length = 120;
              formatter.trim_trailing_whitespace = true;
              formatter.include_document_start = true;
            };
          };

        in
        {
          inherit treefmt;
          # Per-system attributes can be defined here. The self' and inputs'
          # module parameters provide easy access to attributes of the same
          # system.

          checks = {
            # Build the crates as part of `nix flake check` for convenience
            inherit (edgliPackages) lib-edgli;

            # Run clippy (and deny all warnings) on the workspace source,
            # again, reusing the dependency artifacts from above.
            #
            # Note that this is done as a separate derivation so that
            # we can block the CI if there are issues here, but not
            # prevent downstream consumers from building our crate by itself.
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            docs = craneLib.cargoDoc (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );

            # Audit dependencies
            audit = craneLib.cargoAudit {
              inherit src advisory-db;
            };

            # Audit licenses
            licenses = craneLib.cargoDeny {
              inherit src;
            };

            # Run tests with cargo-nextest
            # Consider setting `doCheck = false` on other crate derivations
            # if you do not want the tests to run twice
            test = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                partitions = 1;
                partitionType = "count";
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
              }
            );

          };

          packages = {
            inherit (edgliPackages)
              lib-edgli
              lib-edgli-x86_64-linux
              lib-edgli-aarch64-linux
              lib-edgli-x86_64-darwin
              lib-edgli-aarch64-darwin
              ;
            inherit pre-commit-check;
            default = edgliPackages.lib-edgli;
          };

          devShells.default = craneLib.devShell {
            inherit pre-commit-check;
            # Inherit inputs from checks.
            checks = self.checks.${system};
            # Additional dev-shell environment variables can be set directly
            # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

            # Extra inputs can be added here; cargo and rustc are provided by default.
            packages = [
              pkgs.cargo-machete
              pkgs.cargo-shear
              pkgs.rust-analyzer
            ];

            VERGEN_GIT_SHA = toString (self.shortRev or self.dirtyShortRev);
          };

          devShells.coverage =
            let
              coverageToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
                extensions = [ "llvm-tools-preview" ];
                targets = [ targetForSystem ];
              };
            in
            pkgs.mkShell {
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs =
                [
                  coverageToolchain
                  pkgs.cargo-llvm-cov
                  pkgs.pkgsStatic.openssl
                ]
                ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
            };

          devShells.ci = pkgs.mkShell {
            packages = [ pkgs.zizmor ];
          };

          apps.coverage-unit = {
            type = "app";
            program = toString (
              pkgs.writeShellScript "coverage-unit" ''
                nix develop .#coverage -c cargo llvm-cov --workspace --all-features --lib --lcov --output-path coverage.lcov
              ''
            );
          };

          formatter = config.treefmt.build.wrapper;
        };
      flake = {
        # The usual flake attributes can be defined here, including system-
        # agnostic ones like nixosModule and system-enumerating ones, although
        # those are more easily expressed in perSystem.

      };
    };
}
