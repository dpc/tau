#!/usr/bin/env bash
set -eou pipefail

function job_lint() {
  selfci step start "treefmt"
  if ! treefmt --ci ; then
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
