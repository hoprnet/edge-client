# edgli.nix - HOPR Edge client package definitions
#
# Builds the lib-edgli binary for multiple platforms using nix-lib builders.
# src, depsSrc, and rev are computed internally here.

{
  builders,
  nixLib,
  self,
  lib,
  pkgs,
}:
let
  fs = lib.fileset;
  root = ./..;

  rev = toString (self.shortRev or self.dirtyShortRev);

  depsSrc = nixLib.mkDepsSrc { inherit root fs; };

  src = nixLib.mkSrc { inherit root fs; };

  cargoToml = ./../Cargo.toml;

  buildArgs = {
    inherit
      src
      depsSrc
      rev
      cargoToml
      ;
    extraNativeBuildInputs = [
      pkgs.pkg-config
    ] ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.mold ];
    extraBuildInputs = [
      pkgs.pkgsStatic.openssl
    ] ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];
  };

  buildPackage = builder: args: builder.callPackage nixLib.mkRustPackage (buildArgs // args);
in
{
  lib-edgli-x86_64-linux = buildPackage builders."x86_64-linux" { };
  lib-edgli-aarch64-linux = buildPackage builders."aarch64-linux" { };
  lib-edgli-x86_64-darwin = buildPackage builders."x86_64-darwin" { };
  lib-edgli-aarch64-darwin = buildPackage builders."aarch64-darwin" { };
  lib-edgli = buildPackage builders.local { };
}
