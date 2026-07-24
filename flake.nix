{
  description = "tau";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    flake-utils.url = "github:numtide/flake-utils";
    flakebox = {
      url = "github:rustshop/flakebox?rev=f00197a6545284292defc80140e118231252291b";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    dpc-public-skills = {
      url = "git+https://radicle.dpc.pw/z2HR882B4c4mTdAgdt4SozpdeTuMf.git";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    selfci = {
      url = "git+https://radicle.dpc.pw/z2tDzYbAXxTQEKTGFVwiJPajkbeDU.git";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
      # TODO: temporarily broken because of wild 0.9.0 hackery
      # inputs.flakebox.follows = "flakebox";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      flakebox,
      dpc-public-skills,
      selfci,
      ...
    }@inputs:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # TODO: get rid of custom stuff
        # pkgs = nixpkgs.legacyPackages.${system};
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            flakebox.overlays.default
          ];
        };

        projectName = "tau";
        cargoCrap = pkgs.callPackage ./nix/pkgs/cargo-crap.nix { };
        selfciPkg = selfci.packages.${system}.default;
        selfciMq = selfci.packages.${system}.mq;

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            # Tau's cargo-crap derivations use a locally pinned package and
            # project-specific CI gates rather than Flakebox's integration.
            cargo-crap.enable = false;
            github.ci.buildOutputs = [ ".#ci.workspace" ];
            just.importPaths = [ "justfile.custom.just" ];
            just.rules.watch.enable = false;
            rootDir.".envrc".text = pkgs.lib.mkForce ''
              use flake

              source_env_if_exists .envrc.local
            '';
            toolchain.components = [
              "rustc"
              "cargo"
              "clippy"
              "rust-analyzer"
              "rust-src"
              "llvm-tools"
            ];
          };
        };

        buildPaths = [
          ".cargo-crap.toml"
          "Cargo.toml"
          "Cargo.lock"
          ".config/nextest.toml"
          ".agents/skills"
          "config"
          "crates"
        ];

        cargoManifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        releaseProfile = cargoManifest.profile.release;
        # Selfci captures and replays Nix build logs verbatim. Keep every nextest
        # lane plain and non-interactive so terminal detection cannot add progress
        # frames or ANSI escapes to those persistent logs.
        nextestReporterArgs = "--color never --show-progress none --status-level none --no-input-handler";
        buildSrc =
          # The universal release binary needs parallel LLVM optimization. This
          # evaluation guard prevents normal release builds from silently
          # returning to the multi-minute fat-LTO/one-CGU configuration.
          assert releaseProfile.lto == "thin";
          assert releaseProfile.codegen-units == 16;
          flakeboxLib.filterSubPaths {
            root = builtins.path {
              name = projectName;
              path = ./.;
              filter = path: type: type != "directory" || builtins.baseNameOf path != "specs";
            };
            paths = buildPaths;
          };

        # Placeholders are 40 / 16 raw bytes that the binary embeds via
        # a `static [u8; N]` in `crates/tau-harness/src/version.rs`.
        # The strings below MUST byte-for-byte match those statics, and
        # the substituted values MUST be the same length so `bbe` can
        # patch them in place without shifting any file offsets.
        #
        # Why the unique `__TAU_BUILD…` prefix: short, "ASCII-table-ish"
        # placeholders (e.g. `0123456`) collide with natural byte runs
        # in the binary (base64 alphabets, hex digit tables) and bbe
        # would silently corrupt them.
        tauBuildRevisionPlaceholder = "__TAU_BUILD_GIT_REVISION_PLACEHOLDER____";
        tauBuildDatePlaceholder = "__TAU_BUILD_DATE";
        tauBuildRevision =
          if (self ? rev) && (builtins.stringLength self.rev == 40) then
            self.rev
          else if (self ? dirtyRev) && (builtins.stringLength self.dirtyRev == 46) then
            "${builtins.substring 0 16 self.dirtyRev}00000000${builtins.substring 24 16 self.dirtyRev}"
          else if (self ? dirtyRev) && (builtins.stringLength self.dirtyRev == 40) then
            self.dirtyRev
          else
            tauBuildRevisionPlaceholder;
        tauBuildDate =
          if self ? lastModifiedDate then
            "${builtins.substring 0 4 self.lastModifiedDate}-${builtins.substring 4 2 self.lastModifiedDate}-${
              builtins.substring 6 2 self.lastModifiedDate
            } ${builtins.substring 8 2 self.lastModifiedDate}:${builtins.substring 10 2 self.lastModifiedDate}"
          else
            tauBuildDatePlaceholder;

        replaceTauBuildInfo =
          package:
          pkgs.stdenv.mkDerivation {
            pname = projectName;
            version = package.version;

            dontUnpack = true;
            dontStrip = true;

            nativeBuildInputs = [ pkgs.bbe ];

            # `bbe` itself silently no-ops when its pattern isn't found,
            # which is exactly how the previous LTO-eats-the-placeholder
            # bug shipped. Track per-placeholder hit counts and require
            # at least one substitution across all executables; also
            # assert no placeholder bytes remain after patching.
            installPhase = ''
              cp -a ${package} $out
              chmod -R u+w $out
              revision_hits=0
              date_hits=0
              for path in $(${pkgs.findutils}/bin/find $out -type f -executable); do
                had_revision=0
                had_date=0
                if grep -aqF '${tauBuildRevisionPlaceholder}' "$path"; then
                  had_revision=1
                fi
                if grep -aqF '${tauBuildDatePlaceholder}' "$path"; then
                  had_date=1
                fi
                ${pkgs.bbe}/bin/bbe \
                  -e 's/${tauBuildRevisionPlaceholder}/${tauBuildRevision}/' \
                  -e 's/${tauBuildDatePlaceholder}/${tauBuildDate}/' \
                  "$path" -o ./tmp
                cat ./tmp > "$path"
                if [ "$had_revision" = 1 ]; then
                  if grep -aqF '${tauBuildRevisionPlaceholder}' "$path"; then
                    echo "error: revision placeholder still present in $path after bbe" >&2
                    exit 1
                  fi
                  revision_hits=$((revision_hits + 1))
                fi
                if [ "$had_date" = 1 ]; then
                  if grep -aqF '${tauBuildDatePlaceholder}' "$path"; then
                    echo "error: date placeholder still present in $path after bbe" >&2
                    exit 1
                  fi
                  date_hits=$((date_hits + 1))
                fi
              done
              if [ "$revision_hits" = 0 ]; then
                echo "error: revision placeholder '${tauBuildRevisionPlaceholder}' not found in any executable under $out" >&2
                echo "       (likely the compiler optimized it out — check crates/tau-harness/src/version.rs)" >&2
                exit 1
              fi
              if [ "$date_hits" = 0 ]; then
                echo "error: date placeholder '${tauBuildDatePlaceholder}' not found in any executable under $out" >&2
                echo "       (likely the compiler optimized it out — check crates/tau-harness/src/version.rs)" >&2
                exit 1
              fi
              ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
                # Wild keeps x86_64 Linux links for this large binary fast. Fail
                # rather than allowing a shared build override to disable it again.
                if ${pkgs.binutils}/bin/readelf --file-header "$out/bin/tau" |
                  grep -qF 'Advanced Micro Devices X86-64'
                then
                  if ! ${pkgs.binutils}/bin/readelf --string-dump .comment "$out/bin/tau" |
                    grep -qF 'Linker: Wild '
                  then
                    echo "error: x86_64 Linux $out/bin/tau was not linked with Wild" >&2
                    exit 1
                  fi
                fi
              ''}
            '';
          };

        multiBuild = (flakeboxLib.craneMultiBuild { }) (
          craneLib':
          let
            craneLib = craneLib'.overrideArgs {
              pname = projectName;
              src = buildSrc;
              nativeBuildInputs = [ ];
              env.RUSTDOCFLAGS = "-D warnings";
              env.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
            };
          in
          rec {
            workspaceDeps = craneLib.buildWorkspaceDepsOnly { };

            workspace = craneLib.buildWorkspace {
              cargoArtifacts = workspaceDeps;
            };

            tests = craneLib.cargoNextest {
              cargoArtifacts = workspace;
              cargoNextestExtraArgs = "--workspace ${nextestReporterArgs}";
              # This terminal gate has no downstream Cargo consumer. Exporting
              # its target directory would recompress about 3 GiB after every run.
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ pkgs.ripgrep ];
            };

            # Public provider cassettes are a wire-compatibility gate, separate
            # from scheduler correctness. Nix's build sandbox denies Internet
            # access while the exact filter and no-tests policy prevent skips.
            vcrTests = craneLib.mkCargoDerivation {
              pname = "${projectName}-curated-provider-vcr";
              cargoArtifacts = workspace;
              buildPhaseCargoCommand = ''
                export TAU_VCR=replay-only
                export TAU_VCR_DIR="$PWD/crates/tau-provider-codex/fixtures/provider-vcr"
                export TAU_CURATED_VCR_LANE=1
                cargo nextest run --locked \
                  -p tau-provider-codex \
                  --cargo-profile $CARGO_PROFILE \
                  --no-tests=fail \
                  ${nextestReporterArgs} \
                  -E 'test(/curated_provider_vcr_replay_only_lane/)'
                mkdir -p "$out"
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ pkgs.cargo-nextest ];
              doCheck = false;
            };

            # Always-on fake-provider acceptance. Nix's build sandbox denies
            # Internet access; the exact filter and no-tests policy prevent skips.
            deterministicE2eTests = craneLib.mkCargoDerivation {
              pname = "${projectName}-deterministic-e2e";
              cargoArtifacts = workspace;
              buildPhaseCargoCommand = ''
                # Poison every ambient startup transport. The fixture must ignore
                # them (including runtime settings reloads and secret discovery)
                # and still prove its exact extension allowlist.
                export TAU_ENABLE_EXTENSIONS=core-shell
                export TAU_EXTENSION_CLI_OVERRIDES='["EnableAll"]'
                export TAU_ROLE_CLI_OVERRIDES='["DisableAll"]'
                export TAU_HARNESS_CONFIG_OVERRIDES='[{"key":"agents.default_role","raw_value":"missing"}]'
                export TAU_STARTUP_ROLE=missing
                  env 'TAU_SECRET_BAD@=poison' cargo nextest run --locked \
                    -p tau-e2e-tests \
                    --test deterministic_provider \
                    --test cancellation_liveness \
                    --cargo-profile $CARGO_PROFILE \
                    --no-tests=fail \
                    ${nextestReporterArgs}
                 # The PTY gate must spawn the exact universal binary from this
                 # Cargo profile rather than discovering a user PATH entry.
                 export TAU_E2E_TAU_BIN="$PWD/target/$CARGO_PROFILE/tau"
                 test -x "$TAU_E2E_TAU_BIN"
                 env 'TAU_SECRET_BAD@=poison' cargo nextest run --locked \
                   -p tau-e2e-tests \
                   --test core_resume \
                   --test core_shell_resume \
                   --cargo-profile $CARGO_PROFILE \
                   --no-tests=fail \
                   ${nextestReporterArgs}
                 mkdir -p "$out"
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ pkgs.cargo-nextest ];
              doCheck = false;
            };

            clippy = craneLib.cargoClippy {
              cargoArtifacts = workspaceDeps;
              cargoClippyExtraArgs = "-- -D warnings";
              # Clippy consumes prebuilt dependencies but is a terminal gate;
              # do not export its post-check target directory.
              doInstallCargoArtifacts = false;
            };

            workspaceDepsCcov = craneLib.buildDepsOnly {
              pname = "${projectName}-workspace-ccov";
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo build --locked --workspace --all-targets --profile $CARGO_PROFILE
              '';
              cargoBuildCommand = "dontuse";
              cargoCheckCommand = "dontuse";
              nativeBuildInputs = [ pkgs.cargo-llvm-cov ];
              doCheck = false;
            };

            workspaceCcov = craneLib.buildWorkspace {
              pname = "${projectName}-workspace-ccov";
              cargoArtifacts = workspaceDepsCcov;
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo build --locked --workspace --all-targets --profile $CARGO_PROFILE
              '';
              nativeBuildInputs = [ pkgs.cargo-llvm-cov ];
              doCheck = false;
            };

            testsCcov = craneLib.mkCargoDerivation {
              pname = "${projectName}-tests-ccov";
              cargoArtifacts = workspaceCcov;
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo nextest run --locked --workspace --all-targets --cargo-profile $CARGO_PROFILE ${nextestReporterArgs}
                mkdir -p $out
                cargo llvm-cov report --profile $CARGO_PROFILE --lcov --output-path $out/lcov.info
                test -s $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [
                pkgs.cargo-llvm-cov
                pkgs.cargo-nextest
                pkgs.ripgrep
              ];
              doCheck = false;
            };

            # cargo-crap reads source and explicit LCOV only. Keep cargoArtifacts
            # explicitly null below: enhanced Crane may otherwise infer and unpack
            # compiled artifacts which none of these derivations consume.
            #
            # Regenerate nix/cargo-crap-baseline.json from this derivation after
            # intentional CRAP-score changes land on the mainline.
            crapBaseline = craneLib.mkCargoDerivation {
              pname = "${projectName}-cargo-crap-ccov-baseline";
              cargoArtifacts = null;
              buildPhaseCargoCommand = ''
                test -s ${testsCcov}/lcov.info
                mkdir -p $out
                ${cargoCrap}/bin/cargo-crap \
                  --workspace \
                  --lcov ${testsCcov}/lcov.info \
                  --format json \
                  --output $out/cargo-crap-baseline.json
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ cargoCrap ];
              doCheck = false;
            };

            crapReport = craneLib.mkCargoDerivation {
              pname = "${projectName}-cargo-crap-ccov-report";
              cargoArtifacts = null;
              buildPhaseCargoCommand = ''
                test -s ${testsCcov}/lcov.info
                mkdir -p $out
                ${cargoCrap}/bin/cargo-crap \
                  --workspace \
                  --lcov ${testsCcov}/lcov.info \
                  --threshold 30 \
                  --top 100 \
                  --min 50 \
                  --format markdown \
                  --output $out/cargo-crap.md
                cp ${testsCcov}/lcov.info $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ cargoCrap ];
              doCheck = false;
            };

            crapRegression = craneLib.mkCargoDerivation {
              pname = "${projectName}-cargo-crap-ccov-regression";
              cargoArtifacts = null;
              buildPhaseCargoCommand = ''
                test -s ${testsCcov}/lcov.info
                # Keep this gate focused on severe CRAP-score regressions.
                ${cargoCrap}/bin/cargo-crap \
                  --workspace \
                  --lcov ${testsCcov}/lcov.info \
                  --baseline ${./nix/cargo-crap-baseline.json} \
                  --threshold 1000 \
                  --min 1000 \
                  --format github \
                  --fail-regression
                mkdir -p $out
                cp ${testsCcov}/lcov.info $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ cargoCrap ];
              doCheck = false;
            };

            crapAbsolute = craneLib.mkCargoDerivation {
              pname = "${projectName}-cargo-crap-ccov-absolute";
              cargoArtifacts = null;
              buildPhaseCargoCommand = ''
                test -s ${testsCcov}/lcov.info
                # Catch severe new high-CRAP functions that --fail-regression
                # reports as new but does not fail on.
                ${cargoCrap}/bin/cargo-crap \
                  --workspace \
                  --lcov ${testsCcov}/lcov.info \
                  --min 100 \
                  --format github \
                  --fail-above
                mkdir -p $out
                cp ${testsCcov}/lcov.info $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ cargoCrap ];
              doCheck = false;
            };

            crap = pkgs.runCommand "${projectName}-cargo-crap-ccov" { } ''
              mkdir -p $out
              ln -s ${crapRegression} $out/regression
              ln -s ${crapAbsolute} $out/absolute
              cp ${crapRegression}/lcov.info $out/lcov.info
            '';

            tauDeps = craneLib.buildDepsOnly {
              cargoExtraArgs = "-p tau";
            };

            tau = replaceTauBuildInfo (
              craneLib.buildPackage (
                {
                  cargoArtifacts = tauDeps;
                  cargoExtraArgs = "-p tau";
                }
                // pkgs.lib.optionalAttrs (craneLib.cargoProfile == "release") {
                  # Keep the final command, wall time, and peak RSS visible in
                  # release logs without changing dev/CI profile semantics.
                  cargoBuildCommand = "${pkgs.time}/bin/time -v cargo build --release --locked";
                  nativeBuildInputs = [ pkgs.time ];
                }
              )
            );
          }
        );

        site = pkgs.runCommand "tau-agent-site" { } ''
          mkdir -p $out/share/tau-agent-site
          cp -r ${./site}/* $out/share/tau-agent-site/
        '';

        release-archives =
          pkgs.runCommand "${projectName}-release-archives"
            {
              nativeBuildInputs = [
                pkgs.gnutar
                pkgs.gzip
              ];
            }
            ''
              mkdir -p $out

              archive_dir=${projectName}-${multiBuild.x86_64-linux.release.tau.version}-x86_64-unknown-linux-gnu
              mkdir -p "$archive_dir"
              cp ${multiBuild.x86_64-linux.release.tau}/bin/tau "$archive_dir/tau"
              chmod 755 "$archive_dir/tau"
              tar --sort=name \
                --mtime='@1' \
                --owner=0 \
                --group=0 \
                --numeric-owner \
                -czf $out/$archive_dir.tar.gz \
                "$archive_dir"

              archive_dir=${projectName}-${multiBuild.aarch64-linux.release.tau.version}-aarch64-unknown-linux-gnu
              mkdir -p "$archive_dir"
              cp ${multiBuild.aarch64-linux.release.tau}/bin/tau "$archive_dir/tau"
              chmod 755 "$archive_dir/tau"
              tar --sort=name \
                --mtime='@1' \
                --owner=0 \
                --group=0 \
                --numeric-owner \
                -czf $out/$archive_dir.tar.gz \
                "$archive_dir"
            '';
      in
      {
        packages = {
          default = multiBuild.tau;
          tau = multiBuild.tau;
          site = site;
          "cargo-crap" = cargoCrap;
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          inherit release-archives;
        };

        ci = {
          inherit (multiBuild)
            workspace
            clippy
            tests
            deterministicE2eTests
            vcrTests
            workspaceCcov
            testsCcov
            crapBaseline
            crapReport
            crapRegression
            crapAbsolute
            crap
            ;
        };

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells {
          channel = "latest";
          components = flakeboxLib.config.toolchain.components ++ [
            "rustc-codegen-cranelift-preview"
          ];
          NEXTEST_SHOW_PROGRESS = "none";
          NEXTEST_STATUS_LEVEL = "none";
          TAU_LOG = "tau_ext_shell=debug,tau_harness=debug,info";
          packages = [
            cargoCrap
            selfciMq
            pkgs.cargo-nextest
            pkgs.taplo
            selfciPkg
          ];
          shellHook = ''
            ${dpc-public-skills.packages.${system}.install}/bin/install-dpc-public-skills
          '';
        };
      }
    );
}
