FROM fedora:40

ARG XTASK_RELEASE_TARGET
ENV CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:/usr/local/zig:$PATH

RUN set -eux; \
    dnf install -y --setopt=install_weak_deps=False \
        rpm-build rpmdevtools systemd-rpm-macros \
        dnf-plugins-core cpio \
        pam-devel glibc-devel \
        gcc make pkgconf-pkg-config \
        ca-certificates curl wget tar xz which util-linux \
        clang python3; \
    dnf clean all

RUN set -eux; \
    if [ "$XTASK_RELEASE_TARGET" != "common" ]; then \
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
            sh -s -- --default-toolchain nightly --profile minimal -y; \
        rustup target add "$XTASK_RELEASE_TARGET"; \
        wget -q https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz; \
        tar -xf zig-x86_64-linux-0.16.0.tar.xz; \
        mv zig-x86_64-linux-0.16.0 /usr/local/zig; \
        ln -s /usr/local/zig/zig /usr/local/bin/zig; \
        rm zig-x86_64-linux-0.16.0.tar.xz; \
        cargo install --locked cargo-zigbuild; \
    fi; \
    chmod -R a+rX /opt/cargo /opt/rustup || true

RUN set -eux; \
    if [ "$XTASK_RELEASE_TARGET" != "common" ]; then \
        case "$XTASK_RELEASE_TARGET" in \
            x86_64-unknown-linux-gnu) arch=x86_64 ;; \
            aarch64-unknown-linux-gnu) arch=aarch64 ;; \
            armv7-unknown-linux-gnueabihf) arch=armv7hl ;; \
            i686-unknown-linux-gnu) arch=i686 ;; \
            *) echo "unsupported target $XTASK_RELEASE_TARGET" >&2; exit 1 ;; \
        esac; \
        mkdir -p "/opt/sysroots/$arch" /tmp/rpms; \
        cd /tmp; \
        if [ "$arch" = "i686" ]; then \
            for pkg in pam-devel pam-libs glibc-devel glibc; do \
                dnf download --downloaddir=/tmp/rpms "$pkg.$arch"; \
            done; \
        else \
            for pkg in pam-devel pam-libs glibc-devel glibc; do \
                dnf download --forcearch="$arch" --downloaddir=/tmp/rpms "$pkg"; \
            done; \
        fi; \
        cd "/opt/sysroots/$arch"; \
        for rpm in /tmp/rpms/*."$arch".rpm /tmp/rpms/*.noarch.rpm; do \
            [ -f "$rpm" ] || continue; \
            rpm2cpio "$rpm" | cpio -idmv 2>/dev/null; \
        done; \
        rm -rf /tmp/rpms; \
        chmod -R a+rX /opt/sysroots; \
    fi

COPY xtask/release/rpm/package.sh /opt/genmeta-release/package
RUN chmod +x /opt/genmeta-release/package
