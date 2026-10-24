# Development and check environment (TC-9): a workstation needs only Docker.
FROM ubuntu:26.04@sha256:c098726dd9b9733f3e885f8de0ef375206a311b0a317eadc40d347b1115fb944

ARG APT_SNAPSHOT=20261020T000000Z
ARG RUSTUP_VERSION=1.29.2
ARG RUSTUP_SHA256=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4

# The snapshot service is HTTPS-only and the base image has no CA certificates, so those come
# from the live archive first. --error-on=any: apt-get update otherwise exits 0 on a failed index.
RUN apt-get update --error-on=any \
 && apt-get install -y --no-install-recommends ca-certificates \
 && apt-get update --error-on=any --snapshot "${APT_SNAPSHOT}" \
 && apt-get install -y --no-install-recommends --snapshot "${APT_SNAPSHOT}" \
      build-essential pkg-config curl git \
      libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev \
      xvfb xauth x11-utils x11-xserver-utils xdotool xclip xsel \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
COPY rust-toolchain.toml /tmp/toolchain/rust-toolchain.toml
RUN curl -fsSL -o /tmp/rustup-init \
      "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
 && echo "${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum -c - \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none \
 && cd /tmp/toolchain && rustup toolchain install && rustup show active-toolchain \
 && rm -rf /tmp/rustup-init /tmp/toolchain \
 && chmod -R a+rX /opt/rustup /opt/cargo

# Check tools, built from source at exact versions with their own lockfiles. One pinned nightly,
# used only by cargo-fuzz (libFuzzer needs -Z flags).
ARG FUZZ_NIGHTLY=nightly-2026-10-15
RUN cargo +1.99.1 install --locked --root /opt/cargo \
      cargo-deny@0.20.2 cargo-mutants@27.1.0 cargo-fuzz@0.13.2 \
 && rustup toolchain install "${FUZZ_NIGHTLY}" --profile minimal \
 && rm -rf /opt/cargo/registry /opt/cargo/git \
 && chmod -R a+rX /opt/rustup /opt/cargo

# Added after the Rust layers so that adding it did not rebuild them.
RUN apt-get update --error-on=any --snapshot "${APT_SNAPSHOT}" \
 && apt-get install -y --no-install-recommends --snapshot "${APT_SNAPSHOT}" shellcheck \
 && rm -rf /var/lib/apt/lists/*

# Per-run caches live in the bind-mounted tree (gitignored), so the container runs as the host user.
ENV JACKSTAY_DEV_CONTAINER=1 CARGO_HOME=/work/.cache/cargo \
    RUSTUP_TOOLCHAIN=1.99.1 \
    FUZZ_NIGHTLY=${FUZZ_NIGHTLY}
WORKDIR /work
