ARG XTASK_RELEASE_RUST_IMAGE=rust:bookworm
FROM ${XTASK_RELEASE_RUST_IMAGE}

ARG XTASK_RELEASE_TARGET
ARG XTASK_RELEASE_NIGHTLY=nightly-2026-08-02
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/zig:$PATH

RUN set -eux; \
    apt-get update -qq; \
    apt-get install --assume-yes -qq \
        ca-certificates curl build-essential pkg-config libclang-dev wget python3 \
        dpkg-dev debhelper fakeroot; \
    if [ "$XTASK_RELEASE_TARGET" != "common" ]; then \
        case "$XTASK_RELEASE_TARGET" in \
            x86_64-unknown-linux-gnu) deb=amd64; gnu=x86_64-linux-gnu ;; \
            aarch64-unknown-linux-gnu) deb=arm64; gnu=aarch64-linux-gnu ;; \
            armv7-unknown-linux-gnueabihf) deb=armhf; gnu=arm-linux-gnueabihf ;; \
            i686-unknown-linux-gnu) deb=i386; gnu=i386-linux-gnu ;; \
            *) echo "unsupported target $XTASK_RELEASE_TARGET" >&2; exit 1 ;; \
        esac; \
        dpkg --add-architecture "$deb"; \
        apt-get update -qq; \
        apt-get install --assume-yes -qq "libc-dev:$deb" "libpam0g-dev:$deb" "binutils-$gnu" || \
            apt-get install --assume-yes -qq "libc-dev:$deb" "libpam0g-dev:$deb"; \
    fi

RUN --mount=type=cache,id=genmeta-release-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=genmeta-release-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=genmeta-release-cargo-install-target,target=/tmp/cargo-install-target \
    set -eux; \
    if [ "$XTASK_RELEASE_TARGET" != "common" ]; then \
        if [ "$XTASK_RELEASE_TARGET" = "aarch64-unknown-linux-gnu" ]; then \
            rustup toolchain install "$XTASK_RELEASE_NIGHTLY" --profile minimal; \
            rustup default "$XTASK_RELEASE_NIGHTLY"; \
        fi; \
        rustup target add "$XTASK_RELEASE_TARGET"; \
        case "$(uname -m)" in \
            x86_64) zig_arch=x86_64 ;; \
            aarch64) zig_arch=aarch64 ;; \
            *) echo "unsupported Zig build host: $(uname -m)" >&2; exit 1 ;; \
        esac; \
        zig_archive="zig-$zig_arch-linux-0.16.0.tar.xz"; \
        wget -q --tries=5 --timeout=30 "https://ziglang.org/download/0.16.0/$zig_archive"; \
        tar -xf "$zig_archive"; \
        mv "zig-$zig_arch-linux-0.16.0" /usr/local/zig; \
        ln -s /usr/local/zig/zig /usr/local/bin/zig; \
        rm "$zig_archive"; \
        CARGO_TARGET_DIR=/tmp/cargo-install-target cargo install --locked cargo-zigbuild; \
    fi; \
    chmod -R a+rX /usr/local/cargo /usr/local/rustup

COPY xtask/release/deb/package.sh /opt/genmeta-release/package
RUN chmod +x /opt/genmeta-release/package
