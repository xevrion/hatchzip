{
  lib,
  pkgs,
  stdenv,
  rustPlatform,
  makeWrapper,
  # pkg-config,
  src,
  version ?
    (builtins.fromTOML (
      builtins.readFile "${src}/Cargo.toml"
    )).workspace.package.version,
  enableRar ? false,
}: let
  isLinux = stdenv.hostPlatform.isLinux;
  runtimeLibraries = with pkgs; [
    fontconfig
    libxkbcommon
    libGL
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
    xorg.libxcb
    wayland
  ];
in
  rustPlatform.buildRustPackage {
    pname = "hatchzip-gui";
    inherit src version;

    cargoLock.lockFile = "${src}/Cargo.lock";

    buildAndTestSubdir = "crates/gui";

    buildInputs = lib.optionals isLinux runtimeLibraries;

    nativeBuildInputs = lib.optionals isLinux [
      # pkg-config
      makeWrapper
    ];

    buildFeatures = lib.optionals enableRar ["rar"];

    # The tests lower limits through environment variables that only debug
    # builds read.
    checkType = "debug";
    preBuild = ''
      unset ZSTD_SYS_USE_PKG_CONFIG
    '';
    preCheck = ''
      export HOME="$TMPDIR"
    '';

    postInstall = lib.optionalString enableRar ''
      install -Dm444 ${src}/LICENSE-UNRAR \
        $out/share/licenses/hatchzip-gui/LICENSE-UNRAR
    '';

    postFixup = lib.optionalString isLinux ''
      wrapProgram $out/bin/hatchzip-gui \
        --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
    '';

    meta = {
      description = "HatchZip desktop application";
      homepage = "https://hatchzip.dev";
      license = with lib.licenses;
        AND (
          [(OR [mit asl20])]
          ++ lib.optionals enableRar [unfreeRedistributable]
        );
      maintainers = [];
      mainProgram = "hatchzip-gui";
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  }
