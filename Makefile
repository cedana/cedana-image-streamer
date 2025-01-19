ifndef VERBOSE
.SILENT:
endif

BINARY=cedana-image-streamer
BUILD_SOURCES=Cargo.toml $(shell find . -name '*.proto' -o -name '*.rs')
OUT_DIR=$(PWD)/target/release
SUDO=sudo -E env "PATH=$(PATH)"
VERSION=$(shell git describe --tags --always | sed 's/^v//')

all: install

build: $(OUT_DIR)/$(BINARY)

$(OUT_DIR)/$(BINARY): $(BUILD_SOURCES)
	sed -i "s/^version = \".*\"/version = \"$(VERSION)\"/" Cargo.toml
	cargo build --release --bin $(BINARY)

install: $(OUT_DIR)/$(BINARY)
	$(SUDO) rm -f /usr/local/bin/$(BINARY)
	$(SUDO) cp $(OUT_DIR)/$(BINARY) /usr/local/bin

reset:
	$(SUDO) rm -f /usr/local/bin/$(BINARY)
	rm -f $(OUT_DIR)/$(BINARY)
