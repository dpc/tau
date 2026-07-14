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

  selfci step start "build"
  nix build -L .#ci.workspace

  selfci step start "clippy"
  if ! nix build -L .#ci.clippy ; then
    selfci step fail
  fi

  selfci step start "nextest"
  if ! nix build -L .#ci.tests ; then
    selfci step fail
  fi

  selfci step start "curated provider VCR replay"
  if ! nix build -L .#ci.vcrTests ; then
    selfci step fail
  fi
}

function job_site() {
  selfci step start "site"
  nix build -L .#site
}

function job_coverage() {
  selfci step start "coverage tests"
  if ! nix build -L .#ci.testsCcov ; then
    selfci step fail
  fi

  selfci step start "cargo-crap gates"
  if ! nix build -L .#ci.crap ; then
    >&2 echo "cargo-crap: failed - CRAP regression or high-CRAP function detected; refactor to simplify/decompose and/or increase test coverage of flagged code"
    selfci step fail
  fi
}

case "$SELFCI_JOB_NAME" in
  main)
    selfci job start "lint"
    selfci job start "cargo"
    selfci job start "site"
    selfci job start "coverage"
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
  coverage)
    job_coverage
    ;;
  *)
    echo "Unknown job: $SELFCI_JOB_NAME"
    exit 1
    ;;
esac
