# This context contains trusted packaging tooling only, never selected source.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG RUST_VERSION
ARG RUST_TARGET
ARG RUST_SHA256
ARG NFPM_VERSION
ARG NFPM_ARCH
ARG NFPM_SHA256
RUN set -eux; \
    curl --fail --location --retry 3 \
      "https://static.rust-lang.org/dist/rust-${RUST_VERSION}-${RUST_TARGET}.tar.xz" \
      --output /tmp/rust.tar.xz; \
    echo "${RUST_SHA256}  /tmp/rust.tar.xz" | sha256sum --check --strict; \
    mkdir /tmp/rust; \
    tar -xJf /tmp/rust.tar.xz --strip-components=1 -C /tmp/rust; \
    /tmp/rust/install.sh --prefix=/opt/rust --disable-ldconfig \
      --components="rustc,cargo,rust-std-${RUST_TARGET}"; \
    curl --fail --location --retry 3 \
      "https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_${NFPM_ARCH}.tar.gz" \
      --output /tmp/nfpm.tar.gz; \
    echo "${NFPM_SHA256}  /tmp/nfpm.tar.gz" | sha256sum --check --strict; \
    tar -xzf /tmp/nfpm.tar.gz -C /usr/local/bin nfpm; \
    rm -rf /tmp/rust /tmp/rust.tar.xz /tmp/nfpm.tar.gz
ENV PATH="/opt/rust/bin:/opt/python/cp311-cp311/bin:/opt/rh/gcc-toolset-14/root/usr/bin:/usr/local/bin:/usr/bin:/bin"
# Compile the exact registry archive with its own locked dependency checksums.
# This produces a native manylinux executable, not a Nix-store-linked tool.
RUN set -eux; \
    curl --fail --location --retry 3 \
      https://static.crates.io/crates/cargo-about/cargo-about-0.9.0.crate \
      --output /tmp/cargo-about.crate; \
    echo "0516cb26b5434617e94119d3e0c54b8a805b8fd8c888147dcaa2fa71318b7c52  /tmp/cargo-about.crate" | sha256sum --check --strict; \
    mkdir /tmp/cargo-about; \
    tar -xzf /tmp/cargo-about.crate --strip-components=1 -C /tmp/cargo-about; \
    CARGO_BUILD_JOBS=2 cargo install --locked --features cli --bin cargo-about \
      --path /tmp/cargo-about --root /opt/cargo-about; \
    /opt/cargo-about/bin/cargo-about --version; \
    rm -rf /tmp/cargo-about /tmp/cargo-about.crate /root/.cargo
ENV PATH="/opt/cargo-about/bin:/opt/rust/bin:/opt/python/cp311-cp311/bin:/opt/rh/gcc-toolset-14/root/usr/bin:/usr/local/bin:/usr/bin:/bin"
ENV HOME=/work/home CARGO_HOME=/work/cargo CARGO_TARGET_DIR=/work/target
ENV CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 GIT_NO_REPLACE_OBJECTS=1
ENV PYTHONDONTWRITEBYTECODE=1
WORKDIR /source
