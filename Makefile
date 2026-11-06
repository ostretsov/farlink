# Farlink build commands.
#
#   make              same as `make all`
#   make all          release builds for every platform + the dist/ folder
#   make linux        Linux x86_64   (viewer, helper, server)
#   make linux-arm64  Linux ARM64    (needs an ARM64 machine or a cross linker)
#   make windows      Windows x86_64 (viewer, helper) - cross-built with mingw-w64
#   make dist         lay the builds out the way the server expects (dist/)
#   make test         all automated tests
#   make check        compile-check every platform without building binaries
#   make run-demo     start the viewer with sample computers
#   make clean        remove build outputs
#
# Needs: Rust (rustup). For Windows on Linux: `rustup target add x86_64-pc-windows-gnu`
# and `sudo apt install mingw-w64`.

CARGO   ?= cargo
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

LINUX       := x86_64-unknown-linux-gnu
LINUX_ARM64 := aarch64-unknown-linux-gnu
WINDOWS     := x86_64-pc-windows-gnu

DIST := dist
REL  := $(DIST)/releases

# Programs per platform (the server only runs on Linux).
LINUX_BINS   := farlink farlink-host farlink-server
WINDOWS_BINS := farlink farlink-host

.PHONY: all linux linux-arm64 windows dist package test check run-demo clean version help \
        targets dist-linux dist-linux-arm64 dist-windows

all: linux windows dist

help:
	@sed -n '2,15p' Makefile | sed 's/^# \{0,1\}//'

version:
	@echo $(VERSION)

targets:
	rustup target add $(WINDOWS)

# ---------------------------------------------------------------- builds

linux:
	$(CARGO) build --release --locked --target $(LINUX) $(addprefix --bin ,$(LINUX_BINS))

linux-arm64:
	$(CARGO) build --release --locked --target $(LINUX_ARM64) $(addprefix --bin ,$(LINUX_BINS))

windows:
	$(CARGO) build --release --locked --target $(WINDOWS) $(addprefix --bin ,$(WINDOWS_BINS))

# ---------------------------------------------------------------- dist layout
#
#   dist/farlink-server                       copy dist/ to your server, then:
#   dist/releases/version.txt                 sudo ./farlink-server install --domain ...
#   dist/releases/<target>/<programs>

dist-linux:
	@test -f target/$(LINUX)/release/farlink || { echo "run 'make linux' first"; exit 1; }
	mkdir -p $(REL)/$(LINUX)
	cp target/$(LINUX)/release/farlink target/$(LINUX)/release/farlink-host $(REL)/$(LINUX)/
	cp target/$(LINUX)/release/farlink-server $(DIST)/

dist-linux-arm64:
	@test -f target/$(LINUX_ARM64)/release/farlink || { echo "run 'make linux-arm64' first"; exit 1; }
	mkdir -p $(REL)/$(LINUX_ARM64)
	cp target/$(LINUX_ARM64)/release/farlink target/$(LINUX_ARM64)/release/farlink-host $(REL)/$(LINUX_ARM64)/

dist-windows:
	@test -f target/$(WINDOWS)/release/farlink.exe || { echo "run 'make windows' first"; exit 1; }
	mkdir -p $(REL)/$(WINDOWS)
	cp target/$(WINDOWS)/release/farlink.exe target/$(WINDOWS)/release/farlink-host.exe $(REL)/$(WINDOWS)/

# Copies whatever platforms have been built.
dist:
	mkdir -p $(REL)
	@if [ -f target/$(LINUX)/release/farlink ]; then $(MAKE) --no-print-directory dist-linux; fi
	@if [ -f target/$(LINUX_ARM64)/release/farlink ]; then $(MAKE) --no-print-directory dist-linux-arm64; fi
	@if [ -f target/$(WINDOWS)/release/farlink.exe ]; then $(MAKE) --no-print-directory dist-windows; fi
	echo "$(VERSION)" > $(REL)/version.txt
	cp README.md INSTALL-SERVER.md $(DIST)/
	@echo
	@echo "Farlink $(VERSION) is in ./$(DIST)"
	@echo "First install on a server:  sudo ./farlink-server install --domain remote.example.com"
	@echo "Publish an update later:    sudo farlink-server publish $(VERSION) ./releases --config /etc/farlink-server/config.json"

# One archive with everything (what the GitHub release attaches).
package: dist
	tar -C $(DIST) -czf farlink-$(VERSION).tar.gz .
	@echo "farlink-$(VERSION).tar.gz"

# ---------------------------------------------------------------- checks

test:
	$(CARGO) test --locked

check:
	$(CARGO) check --locked --all-targets
	$(CARGO) check --locked --target $(WINDOWS) --bins --lib

run-demo:
	$(CARGO) run --bin farlink -- --demo

clean:
	$(CARGO) clean
	rm -rf $(DIST) farlink-*.tar.gz
