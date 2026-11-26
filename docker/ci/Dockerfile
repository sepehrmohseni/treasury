# Everything `cargo xtask ci` needs, in one image, so the checks run the same on any Linux or macOS
# machine with Docker, on x86_64 or arm64. Run it through scripts/docker-check.sh, not by hand.

# The Rust release pinned in rust-toolchain.toml; keep the two in step.
ARG RUST_IMAGE=rust:1.100.0-trixie
FROM ${RUST_IMAGE}

# WebKitGTK and friends build the Tauri shell; unixODBC links the Db2 bridge; Xvfb and
# WebKitWebDriver run the desktop smoke test.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
      libayatana-appindicator3-dev librsvg2-dev libxdo-dev unixodbc-dev \
      webkit2gtk-driver xvfb xauth fonts-dejavu-core libgl1-mesa-dri libegl1 libgbm1 \
      xz-utils jq procps iproute2 \
 && rm -rf /var/lib/apt/lists/*

ARG NODE_VERSION=26.13.1
ARG PNPM_VERSION=12.23.0

# Node is checked against the release's SHASUMS256.txt before it is unpacked. Node 26 no longer
# ships corepack, so pnpm comes from the npm registry, which verifies its integrity hash; its
# install script fetches the matching pnpm binary, so it is allowed to run.
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      amd64) arch=x64 ;; \
      arm64) arch=arm64 ;; \
      *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    file="node-v${NODE_VERSION}-linux-${arch}.tar.xz"; \
    cd /tmp; \
    curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/${file}"; \
    curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt" | grep " ${file}\$" | sha256sum -c -; \
    tar -xJf "$file" -C /usr/local --strip-components=1 --no-same-owner; \
    rm "$file"; \
    npm install --global --no-fund --no-audit --allow-scripts=pnpm "pnpm@${PNPM_VERSION}"; \
    pnpm --version; \
    npm cache clean --force

ARG NEXTEST_VERSION=0.9.152
ARG DENY_VERSION=0.22.1
ARG CYCLONEDX_VERSION=0.5.10
ARG GIT_CLIFF_VERSION=2.14.2
ARG LLVM_COV_VERSION=0.9.2
ARG AUDIT_VERSION=0.23.0
# cargo-llvm-cov and cargo-audit publish no checksum files; these are the SHA-256 digests GitHub
# reports for the release assets.
ARG LLVM_COV_SHA256_AMD64=1e0ed5c64f11e9217f7b9d7c776b3125eab26f27d49313919c909881a2fc7cff
ARG LLVM_COV_SHA256_ARM64=150331cd34177e52ec9aed2e32dfe624714030752b5436595281af15c778e38e
ARG AUDIT_SHA256_AMD64=b77993532b253fcbf550996d7716465ec3d734d4e74e7f87bfb65a19add0fb59
ARG AUDIT_SHA256_ARM64=00eff3158429b9629abe8748e971f29f99ee53d4cff54a25072cbb15167bdda8

# Prebuilt cargo tools, each verified against its published checksum before it is unpacked.
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      amd64) t=x86_64; llvm_cov_sha="$LLVM_COV_SHA256_AMD64"; audit_sha="$AUDIT_SHA256_AMD64" ;; \
      arm64) t=aarch64; llvm_cov_sha="$LLVM_COV_SHA256_ARM64"; audit_sha="$AUDIT_SHA256_ARM64" ;; \
      *) echo "unsupported architecture" >&2; exit 1 ;; \
    esac; \
    gh=https://github.com; bin=/usr/local/bin; mkdir -p /tmp/tools; cd /tmp/tools; \
    \
    f="cargo-nextest-${NEXTEST_VERSION}-${t}-unknown-linux-gnu.tar.gz"; \
    curl -fsSLO "$gh/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/$f"; \
    echo "$(curl -fsSL "$gh/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/${f%.tar.gz}.sha256" | cut -d' ' -f1)  $f" | sha256sum -c -; \
    tar -xzf "$f" -C "$bin"; \
    \
    f="cargo-deny-${DENY_VERSION}-${t}-unknown-linux-musl.tar.gz"; \
    curl -fsSLO "$gh/EmbarkStudios/cargo-deny/releases/download/${DENY_VERSION}/$f"; \
    echo "$(curl -fsSL "$gh/EmbarkStudios/cargo-deny/releases/download/${DENY_VERSION}/$f.sha256" | cut -d' ' -f1)  $f" | sha256sum -c -; \
    tar -xzf "$f" --strip-components=1 -C "$bin" "cargo-deny-${DENY_VERSION}-${t}-unknown-linux-musl/cargo-deny"; \
    \
    f="cargo-cyclonedx-${t}-unknown-linux-gnu.tar.xz"; \
    curl -fsSLO "$gh/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-${CYCLONEDX_VERSION}/$f"; \
    echo "$(curl -fsSL "$gh/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-${CYCLONEDX_VERSION}/$f.sha256" | cut -d' ' -f1)  $f" | sha256sum -c -; \
    tar -xJf "$f" --strip-components=1 -C "$bin" "cargo-cyclonedx-${t}-unknown-linux-gnu/cargo-cyclonedx"; \
    \
    f="git-cliff-${GIT_CLIFF_VERSION}-${t}-unknown-linux-gnu.tar.gz"; \
    curl -fsSLO "$gh/orhun/git-cliff/releases/download/v${GIT_CLIFF_VERSION}/$f"; \
    echo "$(curl -fsSL "$gh/orhun/git-cliff/releases/download/v${GIT_CLIFF_VERSION}/$f.sha512" | cut -d' ' -f1)  $f" | sha512sum -c -; \
    tar -xzf "$f" --strip-components=1 -C "$bin" "git-cliff-${GIT_CLIFF_VERSION}/git-cliff"; \
    \
    f="cargo-llvm-cov-${t}-unknown-linux-gnu.tar.gz"; \
    curl -fsSLO "$gh/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/$f"; \
    echo "${llvm_cov_sha}  $f" | sha256sum -c -; \
    tar -xzf "$f" -C "$bin"; \
    \
    f="cargo-audit-${t}-unknown-linux-gnu-v${AUDIT_VERSION}.tgz"; \
    curl -fsSLO "$gh/rustsec/rustsec/releases/download/cargo-audit%2Fv${AUDIT_VERSION}/$f"; \
    echo "${audit_sha}  $f" | sha256sum -c -; \
    tar -xzf "$f" --strip-components=1 -C "$bin" "cargo-audit-${t}-unknown-linux-gnu-v${AUDIT_VERSION}/cargo-audit"; \
    \
    cd /; rm -rf /tmp/tools; \
    chmod 755 "$bin"/cargo-*  "$bin"/git-cliff; \
    cargo nextest --version; cargo deny --version; cargo cyclonedx --version; git-cliff --version; \
    cargo llvm-cov --version; cargo audit --version

RUN rustup component add clippy rustfmt llvm-tools \
 && rustup target add wasm32-unknown-unknown \
 && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"

# The interface is Leptos compiled to WebAssembly (ADR 0011). trunk builds it, wasm-bindgen-cli
# must match the wasm-bindgen version in Cargo.lock, and the Tauri CLI bundles the app. They are
# built from crates.io at pinned versions with --locked, so cargo verifies every source checksum.
ARG TRUNK_VERSION=0.22.2
ARG WASM_BINDGEN_VERSION=0.2.130
ARG TAURI_CLI_VERSION=2.12.6
RUN cargo install --locked --root /usr/local "trunk@${TRUNK_VERSION}" \
 && cargo install --locked --root /usr/local "wasm-bindgen-cli@${WASM_BINDGEN_VERSION}" \
 && cargo install --locked --root /usr/local "tauri-cli@${TAURI_CLI_VERSION}" \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git" \
 && trunk --version && wasm-bindgen --version && cargo tauri --version

# tauri-driver puts WebDriver in front of WebKitWebDriver for the desktop end-to-end test. Its own
# layer, so changing it does not rebuild the tools above.
ARG TAURI_DRIVER_VERSION=2.1.0
RUN cargo install --locked --root /usr/local "tauri-driver@${TAURI_DRIVER_VERSION}" \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

# wasm-opt, at the version CI and the release builds use (see install-binaryen.sh).
COPY install-binaryen.sh /usr/local/lib/treasury/install-binaryen.sh
RUN /usr/local/lib/treasury/install-binaryen.sh /usr/local/binaryen \
 && ln -s /usr/local/binaryen/bin/wasm-opt /usr/local/bin/wasm-opt

# The container runs as the caller's own user id, so caches and the account file must be
# writable by any id. The caches live in named volumes created from these directories.
RUN mkdir -p /cache/cargo /cache/target /cache/pnpm /cache/home /work \
 && chmod 1777 /cache/cargo /cache/target /cache/pnpm /cache/home /work \
 && chmod 666 /etc/passwd

ENV CARGO_HOME=/cache/cargo \
    CARGO_TARGET_DIR=/cache/target \
    CARGO_TERM_COLOR=always \
    HOME=/cache/home \
    npm_config_store_dir=/cache/pnpm \
    PATH=/usr/local/cargo/bin:$PATH

COPY entrypoint.sh /usr/local/bin/treasury-ci
RUN chmod 755 /usr/local/bin/treasury-ci

ENTRYPOINT ["/usr/local/bin/treasury-ci"]
CMD ["ci"]
