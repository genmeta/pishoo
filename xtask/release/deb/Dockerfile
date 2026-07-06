FROM debian:bookworm

ARG XTASK_RELEASE_TARGET
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:/usr/local/zig:$PATH

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

RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- --default-toolchain nightly --profile minimal -y; \
    if [ "$XTASK_RELEASE_TARGET" != "common" ]; then rustup target add "$XTASK_RELEASE_TARGET"; fi; \
    wget -q https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz; \
    tar -xf zig-x86_64-linux-0.16.0.tar.xz; \
    mv zig-x86_64-linux-0.16.0 /usr/local/zig; \
    ln -s /usr/local/zig/zig /usr/local/bin/zig; \
    rm zig-x86_64-linux-0.16.0.tar.xz; \
    cargo install --locked cargo-zigbuild; \
    chmod -R a+rX /opt/cargo /opt/rustup

COPY xtask/release/deb/package.sh /opt/genmeta-release/package
RUN chmod +x /opt/genmeta-release/package
