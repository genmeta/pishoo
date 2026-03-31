# gateway/Makefile — Build & package pishoo for deb + homebrew
#
# Usage:
#   make deb              Build all deb architectures
#   make deb-amd64        Build deb for a single architecture
#   make homebrew          Build homebrew formula (macOS only)
#   make upload-deb        Upload deb packages
#   make upload-homebrew   Upload homebrew formula + archives
#   make -n deb            Dry run
#   make -j4 deb           Parallel build

BUILDX_DIR := $(or $(BUILDX_DIR),$(HOME)/code/reimu/genmeta-buildx)
include $(BUILDX_DIR)/archs.mk

# --- Project metadata (from Cargo.toml) ---
NAME     := pishoo
FEATURES := --features sshd
VERSION  := $(shell cargo metadata --no-deps --format-version=1 \
              | python3 -c "import sys,json; pkgs=json.loads(sys.stdin.read())['packages']; print(next(p['version'] for p in pkgs if p['name']=='$(NAME)'))")
DESCRIPTION := $(shell cargo metadata --no-deps --format-version=1 \
              | python3 -c "import sys,json; pkgs=json.loads(sys.stdin.read())['packages']; print(next(p['description'] for p in pkgs if p['name']=='$(NAME)'))")
HOMEPAGE := $(shell cargo metadata --no-deps --format-version=1 \
              | python3 -c "import sys,json; pkgs=json.loads(sys.stdin.read())['packages']; print(next(p['homepage'] or '' for p in pkgs if p['name']=='$(NAME)'))")

# --- Deb configuration ---
DEB_ARCHS    := amd64 arm64 armhf
DEB_REMOTE   := ubuntu@download.genmeta.net:/data/wwwroot/ppa/deb/main
DOCKER_IMG   := $(DOCKER_IMG_UBUNTU20)
DEBS_DIR     := $(CURDIR)/debs/pishoo
PISHOO_COMMON_VERSION := 0.2.1

# --- Homebrew configuration ---
BREW_ARCHS     := apple intel
BREW_REMOTE    := ubuntu@download.genmeta.net:/data/wwwroot/homebrew
BREW_DL_URL    := https://download.genmeta.net/homebrew
BREW_CONTENT   := pishoo/homebrew_content.rb
BREW_OUTPUT    := homebrew-genmeta/pishoo.rb

# --- Docker helpers ---
# Volume mounts for cargo toolchain
CARGO_MOUNTS = \
	-v $(CARGO_HOME_DIR)/config.toml:/cargo/config.toml \
	-v $(CARGO_HOME_DIR)/git:/cargo/git \
	-v $(CARGO_HOME_DIR)/registry:/cargo/registry

# Generate Dockerfile, build image, return tag
# $(1) = arch alias
define docker_image_tag
$(DOCKER_IMG)-$(ARCH_$(1)_LLVM):$(NAME)
endef

define docker_ensure_image
	@# Generate Dockerfile for this arch
	@mkdir -p base_images_cache
	@echo 'FROM $(DOCKER_IMG)' > base_images_cache/$(NAME)-$(1).dockerfile
	@echo 'RUN /cargo/bin/rustup target add $(ARCH_$(1)_LLVM)' >> base_images_cache/$(NAME)-$(1).dockerfile
	@echo 'RUN dpkg --add-architecture $(ARCH_$(1)_DEB) && apt-get update && apt-get install --assume-yes libc-dev:$(ARCH_$(1)_DEB) libpam0g-dev:$(ARCH_$(1)_DEB)' >> base_images_cache/$(NAME)-$(1).dockerfile
	$(CONTAINER_ENGINE) buildx build \
		-t $(call docker_image_tag,$(1)) \
		-f base_images_cache/$(NAME)-$(1).dockerfile \
		$(BUILDX_DIR)/base_images
	@# Also ensure base image is built
	$(CONTAINER_ENGINE) buildx build \
		-t $(DOCKER_IMG) \
		-f $(BUILDX_DIR)/base_images/$(DOCKER_IMG).dockerfile \
		$(BUILDX_DIR)/base_images
endef

# ============================================================
# Deb targets
# ============================================================

define deb_target
.PHONY: deb-$(1)
deb-$(1): | $(DEBS_DIR)
	$(call docker_ensure_image,$(1))
	$(CONTAINER_ENGINE) run --rm \
		$(CARGO_MOUNTS) \
		-v $(CURDIR):/app \
		-v $(DEBS_DIR):/debs \
		-e PISHOO_WORKER_BIN=/usr/lib/pishoo/pishoo-worker \
		-e PISHOO_SSH_SESSION_BIN=/usr/lib/pishoo/pishoo-ssh-session \
		$(call docker_image_tag,$(1)) \
		bash -c "\
			source /cargo/env; \
			export RUSTFLAGS=\"$$$${RUSTFLAGS:-} -L /usr/lib/$(ARCH_$(1)_GNU)\"; \
			cargo zigbuild --release --target $(ARCH_$(1)_LLVM) -p pishoo $(FEATURES); \
			cargo deb -p pishoo --target $(ARCH_$(1)_LLVM) --no-build --no-strip; \
			cp target/$(ARCH_$(1)_LLVM)/debian/*.deb /debs/"
endef

$(foreach arch,$(DEB_ARCHS),$(eval $(call deb_target,$(arch))))

# pishoo-common is an arch-independent config package
.PHONY: deb-common
deb-common: | $(DEBS_DIR)
	@DEBIAN_FILE=pishoo-common_$(PISHOO_COMMON_VERSION)-1_all.deb; \
	if [ ! -f $(DEBS_DIR)/$$DEBIAN_FILE ]; then \
		mkdir -p /tmp/pishoo-common/DEBIAN; \
		cp -r pishoo/pkg/common/* /tmp/pishoo-common/; \
		cp -r pishoo/pkg/debian/* /tmp/pishoo-common/DEBIAN/; \
		sed -i "s/Version:.*/Version: $(PISHOO_COMMON_VERSION)-1/" /tmp/pishoo-common/DEBIAN/control; \
		dpkg-deb -b /tmp/pishoo-common $(DEBS_DIR)/$$DEBIAN_FILE; \
	else \
		echo "Package $$DEBIAN_FILE already exists, skipping"; \
	fi

.PHONY: deb
deb: $(addprefix deb-,$(DEB_ARCHS)) deb-common ## Build all deb packages

$(DEBS_DIR):
	mkdir -p $@

# ============================================================
# Homebrew targets (macOS native build)
# ============================================================

define brew_build_target
.PHONY: brew-build-$(1)
brew-build-$(1):
	cargo build --release --target $(ARCH_$(1)_LLVM) -p pishoo $(FEATURES)
	@mkdir -p target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula
	cp target/$(ARCH_$(1)_LLVM)/release/pishoo     target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/
	cp target/$(ARCH_$(1)_LLVM)/release/pishoo-worker target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/
	cp target/$(ARCH_$(1)_LLVM)/release/pishoo-ssh-session target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/
	cp pishoo/pkg/common/etc/pishoo/pishoo.conf target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/
	sed -i '' 's;/etc;etc;g' target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/pishoo.conf
	cp pishoo/pkg/common/etc/pishoo/mime.types  target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula/
	tar czf target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/$(NAME)_$(VERSION)_$(1).tar.gz \
		-C target/$(ARCH_$(1)_LLVM)/homebrew/$(NAME)/formula .
endef

$(foreach arch,$(BREW_ARCHS),$(eval $(call brew_build_target,$(arch))))

.PHONY: homebrew
homebrew: $(addprefix brew-build-,$(BREW_ARCHS)) ## Build all archs + generate formula
	@mkdir -p homebrew-genmeta
	python3 $(BUILDX_DIR)/gen_formula.py \
		--name "$(NAME)" --version "$(VERSION)" \
		--description "$(DESCRIPTION)" \
		--homepage "$(HOMEPAGE)" \
		--content-file "$(BREW_CONTENT)" \
		--download-url "$(BREW_DL_URL)" \
		$(foreach arch,$(BREW_ARCHS),--arch $(arch):target/$(ARCH_$(arch)_LLVM)/homebrew/$(NAME)/$(NAME)_$(VERSION)_$(arch).tar.gz) \
		--output "$(BREW_OUTPUT)"

# ============================================================
# Upload targets
# ============================================================

.PHONY: upload-deb
upload-deb: ## Upload deb packages
	$(RSYNC) $(DEBS_DIR)/*.deb $(DEB_REMOTE)

.PHONY: upload-homebrew
upload-homebrew: ## Upload homebrew formula + archives
	$(RSYNC) $(BREW_OUTPUT) \
		$(foreach arch,$(BREW_ARCHS),target/$(ARCH_$(arch)_LLVM)/homebrew/$(NAME)/$(NAME)_$(VERSION)_$(arch).tar.gz) \
		$(BREW_REMOTE)

# ============================================================
# Convenience
# ============================================================

.PHONY: all
all: deb homebrew ## Build everything

.PHONY: upload
upload: upload-deb upload-homebrew ## Upload everything

.PHONY: clean
clean:
	rm -rf $(DEBS_DIR) base_images_cache homebrew-genmeta
	rm -rf target/*/homebrew

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*##' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'
