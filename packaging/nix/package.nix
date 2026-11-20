{
  lib,
  stdenv,
  rustPlatform,
  installShellFiles,
  versionCheckHook,
  src,
  version ?
    (builtins.fromTOML (
      builtins.readFile "${src}/Cargo.toml"
    )).workspace.package.version,
  enableRar ? false,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "hatchzip";
  inherit src version;

  cargoLock.lockFile = "${src}/Cargo.lock";

  buildAndTestSubdir = "crates/cli";

  # The tests lower limits through environment variables that only debug
  # builds read.
  checkType = "debug";
  preBuild = ''
    unset ZSTD_SYS_USE_PKG_CONFIG
  '';
  preCheck = ''
    export HOME="$TMPDIR"
  '';

  nativeBuildInputs = [installShellFiles];

  # Enable RAR feature only if enableRar is selected
  buildFeatures = lib.optionals enableRar [
    "rar"
  ];

  # Upstream's own installers also put the short name `hz` on PATH.
  postInstall =
    ''
      ln -s hatchzip $out/bin/hz
    ''
    + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      installShellCompletion --cmd hatchzip \
        --bash <($out/bin/hatchzip completions bash) \
        --fish <($out/bin/hatchzip completions fish) \
        --zsh <($out/bin/hatchzip completions zsh)
      $out/bin/hatchzip man > hatchzip.1
      installManPage hatchzip.1
    ''
    + lib.optionalString enableRar ''
      install -Dm444 ${src}/LICENSE-UNRAR \
        $out/share/licenses/hatchzip/LICENSE-UNRAR
    '';

  nativeInstallCheckInputs = [versionCheckHook];
  doInstallCheck = true;

  meta = {
    description = "Extract huge archives in place, needing free space only for the contents";
    homepage = "https://hatchzip.dev";
    changelog = "https://github.com/xevrion/hatchzip/releases/tag/v${finalAttrs.version}";
    license = with lib.licenses;
      AND (
        [(OR [mit asl20])]
        ++ lib.optionals enableRar [unfreeRedistributable]
      );
    maintainers = [];
    mainProgram = "hatchzip";
  };
})
