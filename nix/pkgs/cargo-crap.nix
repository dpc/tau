{
  lib,
  rustPlatform,
  fetchFromGitHub,
}:

rustPlatform.buildRustPackage rec {
  pname = "cargo-crap";
  version = "0.6.1";

  src = fetchFromGitHub {
    owner = "minikin";
    repo = "cargo-crap";
    tag = "v${version}";
    hash = "sha256-8Ex8m/xdrh4PrWlZoWoBm/K0kXrY1NN0lOb1GsL5OIA=";
  };

  # Use the GitHub tag so upstream's full test fixtures are available. Keep the
  # audited crates.io lockfile for reproducible dependency resolution.
  cargoLock.lockFile = ./cargo-crap-Cargo.lock;

  postPatch = ''
    cp ${./cargo-crap-Cargo.lock} Cargo.lock
  '';

  meta = {
    description = "Change Risk Anti-Patterns (CRAP) metric for Rust projects";
    homepage = "https://github.com/minikin/cargo-crap";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "cargo-crap";
  };
}
