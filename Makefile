CRATE_DIR := crates/mrm
BIN       := $(CRATE_DIR)/target/release/mrm
USER_BIN  := $(HOME)/.cargo/bin/mrm
SYS_BIN   := /usr/local/bin/mrm
SERVICE   := mrm.service

.PHONY: all build install install-user install-system restart-daemon clean check

all: build

build:
	cd $(CRATE_DIR) && cargo build --release

check:
	cd $(CRATE_DIR) && cargo check

install: install-user install-system restart-daemon

install-user: build
	cd $(CRATE_DIR) && cargo install --path . --force

install-system: build
	sudo install -m 755 $(BIN) $(SYS_BIN)

restart-daemon:
	-systemctl --user restart $(SERVICE)

clean:
	cd $(CRATE_DIR) && cargo clean
