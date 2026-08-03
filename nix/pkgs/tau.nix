{
  lib,
  rustPlatform,
  bbe,
  testers,
  buildRevision ? null,
  buildDate ? null,
  buildDirty ? null,
}:

assert buildRevision == null || builtins.stringLength buildRevision == 40;
assert buildDate == null || builtins.stringLength buildDate == 16;
assert buildDirty == null || builtins.isBool buildDirty;

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tau";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../.agents/skills
      ../../config
      ../../crates
      ../../Cargo.lock
      ../../Cargo.toml
    ];
  };

  cargoLock = {
    lockFile = ../../Cargo.lock;
  };

  cargoBuildFlags = [
    "--package"
    "tau"
  ];

  nativeBuildInputs = [ bbe ];

  postFixup = ''
    ${lib.optionalString (buildRevision != null) ''
      if ! grep -aqF '__TAU_BUILD_GIT_REVISION_PLACEHOLDER____' "$out/bin/tau"; then
        echo "error: Tau build revision placeholder was not found" >&2
        exit 1
      fi
      bbe \
        -e 's/__TAU_BUILD_GIT_REVISION_PLACEHOLDER____/${buildRevision}/' \
        "$out/bin/tau" -o "$out/bin/tau.tmp"
      cat "$out/bin/tau.tmp" > "$out/bin/tau"
      rm "$out/bin/tau.tmp"
      if grep -aqF '__TAU_BUILD_GIT_REVISION_PLACEHOLDER____' "$out/bin/tau"; then
        echo "error: Tau build revision placeholder remains after patching" >&2
        exit 1
      fi
    ''}

    ${lib.optionalString (buildDate != null) ''
      if ! grep -aqF '__TAU_BUILD_DATE' "$out/bin/tau"; then
        echo "error: Tau build date placeholder was not found" >&2
        exit 1
      fi
      bbe \
        -e 's/__TAU_BUILD_DATE/${buildDate}/' \
        "$out/bin/tau" -o "$out/bin/tau.tmp"
      cat "$out/bin/tau.tmp" > "$out/bin/tau"
      rm "$out/bin/tau.tmp"
      if grep -aqF '__TAU_BUILD_DATE' "$out/bin/tau"; then
        echo "error: Tau build date placeholder remains after patching" >&2
        exit 1
      fi
    ''}

    ${lib.optionalString (buildDirty != null) ''
      if ! grep -aqF '__TAU_BUILD_DIRTY' "$out/bin/tau"; then
        echo "error: Tau dirty-state placeholder was not found" >&2
        exit 1
      fi
      bbe \
        -e 's/__TAU_BUILD_DIRTY/${if buildDirty then "modified_________" else "clean____________"}/' \
        "$out/bin/tau" -o "$out/bin/tau.tmp"
      cat "$out/bin/tau.tmp" > "$out/bin/tau"
      rm "$out/bin/tau.tmp"
      if grep -aqF '__TAU_BUILD_DIRTY' "$out/bin/tau"; then
        echo "error: Tau dirty-state placeholder remains after patching" >&2
        exit 1
      fi
    ''}
  '';

  doCheck = false;

  passthru.tests.version = testers.testVersion { package = finalAttrs.finalPackage; };

  meta = {
    description = "Minimal Unix-first coding agent";
    homepage = "https://github.com/dpc/tau-agent";
    license = lib.licenses.mpl20;
    mainProgram = "tau";
    platforms = lib.platforms.unix;
  };
})
