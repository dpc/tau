# `read_image` visual-fidelity oracle

The experimental overview profile is available for explicitly coarse
inspection, but bare `read_image` remains high. Overview must not become the
default or general workflow guidance until this opt-in live oracle passes. The
oracle is deliberately outside hermetic CI because it contacts configured
models and measures model behavior rather than raster geometry.

Deterministic unit fixtures lock the transforms themselves: representative
desktop, square, and long-page sources quantify high and overview output
dimensions and patches; a native 800×400 crop proves crop-before-resize; and an
EXIF-rotated JPEG proves regions address the oriented source. The opt-in E2E
fixture adds two large color panels and a five-bar source-scale target.

Build the candidate first, then run the smoke oracle with an explicitly selected
image-capable Sol, Terra, or Luna profile, a unique trial id, and a new empty
private cassette directory. The test rejects replay mode, a non-empty cassette
directory, and an unpinned binary:

```sh
cargo build -p dpc-tau
trial="$(date -u +%Y%m%dT%H%M%S)-sol-01"
mkdir "/private/path/read-image-oracle-$trial"
TAU_IMAGE_FIDELITY_ORACLE=1 \
TAU_IMAGE_FIDELITY_TRIAL_ID="$trial" \
TAU_VCR=record-if-missing \
TAU_VCR_DIR="/private/path/read-image-oracle-$trial" \
TAU_E2E_MODEL=<profile/model> \
TAU_E2E_TAU_BIN="$(pwd)/target/debug/tau" \
cargo test -p dpc-tau-e2e-tests --test image_fidelity -- --nocapture
```

Cassettes contain provider traffic and must be handled as private test data.
Replay verifies ordinary VCR integration but the oracle intentionally refuses
it because replay is not new live-fidelity evidence. Preserve each unique trial
directory with its dated trial id and record the command, candidate change id,
model route, and three isolated outcomes. Each profile uses an independent
provider turn, and the test checks byte-free terminal transform metadata so a
wrong mode or crop cannot borrow answers from a retained high image.

Before changing the default or normative guidance, run at least ten fresh live
trials on each supported Sol/Terra/Luna route. The fixed smoke fixture must pass
every trial. Extend the same manifest with 1280×900, 1920×1080, 2560×1440,
3840×2160, 390×844, 430×932, and long-page captures at DPR 1 and 2, covering
8/10/12/14/16-pixel Latin and non-Latin text, one/two-pixel borders and offsets,
overflow, icons, subtle contrast, and one-pixel before/after defects. Record
profile, prepared dimensions, patches, canonical bytes, latency, exact answer,
and pass/fail for each trial.

Overview may be recommended only for coarse layout/presence when it reaches at
least 98% correct and has zero misses on the designated major overlap/overflow
set. Any text size claimed by guidance must reach 99% exact-string accuracy.
Fine clipping, localization, spacing, and diff claims remain high/native-crop
tasks regardless of the coarse gate. Store the dated manifest with the proposal
that changes guidance so review can reproduce the decision.
