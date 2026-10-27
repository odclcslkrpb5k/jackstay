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

# The conformance matrix's clients (docs/conformance.md): each toolkit's own clipboard code, and
# xterm. Appended for the same reason. nss_wrapper gives the tray tests' dbus-daemon a user entry
# under any uid the container runs as (CI's has none in /etc/passwd).
RUN apt-get update --error-on=any --snapshot "${APT_SNAPSHOT}" \
 && apt-get install -y --no-install-recommends --snapshot "${APT_SNAPSHOT}" \
      libgtk-3-dev libgtk-4-dev qt6-base-dev qt6-qpa-plugins libxcb-cursor0 \
      xterm xfonts-base fonts-dejavu-core \
      libnss-wrapper \
 && rm -rf /var/lib/apt/lists/*

# The matrix's application tier (docs/conformance.md): the owner's applications at the owner's
# versions. LibreOffice and xfce4-terminal from the snapshot; Firefox (the archive's is a snap
# stub), Chrome for Testing (Ubuntu's Chromium is a snap) and VS Code (Electron) as their
# makers' builds, checksum-pinned, with the libraries they load.
ARG FIREFOX_VERSION=156.0
ARG FIREFOX_SHA256=1d44cd02351c307c3e19061ea2a4d18a30f236e6be862b94f2282564afdb0167
ARG CHROME_VERSION=155.0.8059.39
ARG CHROME_SHA256=55672d1f392fd3e7b7a08621b6e804e6bcb39d40cf155504abb74b3a021ea8ea
ARG VSCODE_VERSION=1.137.0
ARG VSCODE_SHA256=8871d637292568e34c2d27daa5320a3b3c805f41f94014dfceafbe6134187fce
RUN apt-get update --error-on=any --snapshot "${APT_SNAPSHOT}" \
 && apt-get install -y --no-install-recommends --snapshot "${APT_SNAPSHOT}" \
      xz-utils unzip \
      libreoffice-writer libreoffice-gtk3 python3-uno xfce4-terminal \
      libasound2t64 libnss3 libgbm1 libxss1 libxkbfile1 libsecret-1-0 libcups2t64 \
      libatk-bridge2.0-0t64 libvulkan1 libxcomposite1 libxdamage1 libxrandr2 libcurl4t64 \
      fonts-liberation \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /opt/apps /tmp/apps && cd /tmp/apps \
 && curl -fsSLo firefox.tar.xz \
      "https://ftp.mozilla.org/pub/firefox/releases/${FIREFOX_VERSION}/linux-x86_64/en-US/firefox-${FIREFOX_VERSION}.tar.xz" \
 && curl -fsSLo chrome.zip \
      "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-linux64.zip" \
 && curl -fsSLo vscode.tar.gz "https://update.code.visualstudio.com/${VSCODE_VERSION}/linux-x64/stable" \
 && printf '%s  %s\n' "${FIREFOX_SHA256}" firefox.tar.xz "${CHROME_SHA256}" chrome.zip \
      "${VSCODE_SHA256}" vscode.tar.gz | sha256sum -c - \
 && cd /opt/apps && tar xJf /tmp/apps/firefox.tar.xz && unzip -q /tmp/apps/chrome.zip \
 && tar xzf /tmp/apps/vscode.tar.gz && rm -rf /tmp/apps && chmod -R a+rX /opt/apps

# Xfce's settings daemon, whose own clipboard manager Jackstay cannot run beside
# (crates/jackstayd/tests/xfsettingsd.rs). Appended for the same reason.
RUN apt-get update --error-on=any --snapshot "${APT_SNAPSHOT}" \
 && apt-get install -y --no-install-recommends --snapshot "${APT_SNAPSHOT}" xfce4-settings \
 && rm -rf /var/lib/apt/lists/*

# Per-run caches live in the bind-mounted tree (gitignored), so the container runs as the host user.
ENV JACKSTAY_DEV_CONTAINER=1 CARGO_HOME=/work/.cache/cargo \
    RUSTUP_TOOLCHAIN=1.99.1 \
    FUZZ_NIGHTLY=${FUZZ_NIGHTLY}
WORKDIR /work
