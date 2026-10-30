{
  lib,
  stdenv,
  rustPlatform,
  fetchFromGitHub,
  installShellFiles,
  versionCheckHook,
  nix-update-script,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "hatchzip";
  version = "0.1.8";

  src = fetchFromGitHub {
    owner = "xevrion";
    repo = "hatchzip";
    tag = "v${finalAttrs.version}";
    hash = "sha256-FrbIsX7mgdZEDpFogT+E1dxq6CrMvGwJNB05NdJNNy0=";
  };

  cargoHash = "sha256-LgGWDN+Dro56ujqbVvbrWKq+0aE1+uNpiK2vl5+2JDE=";

  buildAndTestSubdir = "crates/cli";

  # The tests lower limits through environment variables that only debug
  # builds read.
  checkType = "debug";

  nativeBuildInputs = [ installShellFiles ];

  # Upstream's own installers also put the short name `hz` on PATH.
  postInstall = ''
    ln -s hatchzip $out/bin/hz
  ''
  + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    installShellCompletion --cmd hatchzip \
      --bash <($out/bin/hatchzip completions bash) \
      --fish <($out/bin/hatchzip completions fish) \
      --zsh <($out/bin/hatchzip completions zsh)
    $out/bin/hatchzip man > hatchzip.1
    installManPage hatchzip.1
  '';

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  passthru.updateScript = nix-update-script { };

  meta = {
    description = "Extract huge archives in place, needing free space only for the contents";
    homepage = "https://hatchzip.dev";
    changelog = "https://github.com/xevrion/hatchzip/releases/tag/v${finalAttrs.version}";
    license =
      with lib.licenses;
      OR [
        mit
        asl20
      ];
    maintainers = [ ];
    mainProgram = "hatchzip";
  };
})
