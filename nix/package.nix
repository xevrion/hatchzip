{
  lib,
  rustPlatform,
  pkg-config,
  zstd,
  src,
  enableUnfree ? false,
}:
rustPlatform.buildRustPackage {
  pname = "hatchzip";
  version =
    (builtins.fromTOML (
      builtins.readFile "${src}/Cargo.toml"
    )).workspace.package.version;

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  buildType = "release";
  checkType = "debug";

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = [
    zstd
  ];

  cargoBuildFlags = [
    "--package=hatchzip"
  ];

  cargoInstallFlags = [
    "--path=."
    "--package=hatchzip"
  ];

  cargoTestFlags = [
    "--package=hatchzip"
  ];

  buildFeatures = lib.optionals enableUnfree [
    "rar"
  ];

  postInstall = lib.optionalString enableUnfree ''
    install -Dm444 ${src}/LICENSE-UNRAR \
      $out/share/licenses/hatchzip/LICENSE-UNRAR
  '';

  meta = {
    description = "A modern archive manager";
    homepage = "https://hatchzip.dev";
    license = [lib.licenses.mit] ++ lib.optionals enableUnfree [lib.licenses.unfreeRedistributable];
    mainProgram = "hatchzip";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
