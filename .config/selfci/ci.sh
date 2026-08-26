#!/usr/bin/env bash
set -eou pipefail

function job_lint() {
  selfci step start "ast-grep rule tests"
  if ! ast-grep test --config sgconfig.yml; then
    selfci step fail
  fi

  selfci step start "ast-grep scan"
  if ! ast-grep scan --error --config sgconfig.yml; then
    selfci step fail
  fi
  if ! .config/ast-grep/test-debug-assert-acknowledgments.sh; then
    selfci step fail
  fi
  if ! .config/ast-grep/test-path-filters.sh; then
    selfci step fail
  fi
  selfci step start "Rust physical line limit"
  if ! .config/selfci/test-rust-physical-line-limit.sh; then
    selfci step fail
  fi
  if ! .config/selfci/check-rust-physical-line-limit.sh; then
    selfci step fail
  fi

  selfci step start "treefmt"
  if ! treefmt --ci ; then
    selfci step fail
  fi

  selfci step start "quota extractor fixtures"
  if ! python3 .agents/skills/tau-qodq/test_extract_quota.py; then
    selfci step fail
  fi
}

function job_cargo() {
  selfci step start "Cargo.lock up-to-date"
  if ! cargo update --workspace --locked -q; then
    selfci step fail
  fi

  # Submit independent checks together so Nix can schedule them concurrently
  # after their shared workspace build completes.
  selfci step start "Nix cargo checks"
  if ! nix build -L --no-link \
    .#ci.workspace \
    .#ci.clippy \
    .#ci.tests
  then
    selfci step fail
  fi

  if [[ "${TAU_CI_FULL:-false}" == "true" ]]; then
    # The report inventories current debt while the aggregate applies both
    # blocking CRAP gates. Baselines are regenerated only after accepted,
    # intentional score changes, not on every full CI run.
    selfci step start "Nix cargo-crap checks"
    if ! nix build -L --no-link \
      .#ci.crapReport \
      .#ci.crap
    then
      selfci step fail
    fi
  fi
}

function job_site() {
  selfci step start "site"
  nix build -L .#site
}

case "$SELFCI_JOB_NAME" in
  main)
    selfci job start "lint"
    selfci job start "cargo"
    selfci job start "site"
    ;;
  cargo)
    job_cargo
    ;;
  lint)
    export -f job_lint
    nix develop -c bash -c "job_lint"
    ;;
  site)
    job_site
    ;;
  *)
    echo "Unknown job: $SELFCI_JOB_NAME"
    exit 1
    ;;
esac
