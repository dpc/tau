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
ENV HOME=/work/home CARGO_HOME=/work/cargo CARGO_TARGET_DIR=/work/target
ENV CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 GIT_NO_REPLACE_OBJECTS=1
ENV PYTHONDONTWRITEBYTECODE=1
WORKDIR /source
