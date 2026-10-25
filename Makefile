# Nozavolochen framework
#
#   make loader     cross-build xinput9_1_0.dll (mingw, Linux -> Windows)
#   make check      verify embedded mod specs against NMS.exe (read-only)
#                   override with NMS_EXE=/path/to/NMS.exe
#   make dist       pack the framework release zip
#   make assets     regenerate the Nexus page images (header + main)
#   make clean      drop build outputs
#
# Distribution: the framework zip is the required dependency. Mod
# mods ship only their mod.toml (plus static files) and never bundle
# the DLL.

BUILD := build
DIST := dist
LOADER_DIST := loader/dist
DLL := $(LOADER_DIST)/xinput9_1_0.dll

VERSION := $(shell grep '^version' loader/Cargo.toml | head -1 | cut -d'"' -f2)

FRAMEWORK_ZIP := $(DIST)/Nozavolochen Framework $(VERSION).zip

.PHONY: loader check dist assets clean

loader:
	@cd loader && cargo build --target x86_64-pc-windows-gnu --release
	@mkdir -p "$(LOADER_DIST)"
	@cp loader/target/x86_64-pc-windows-gnu/release/xinput9_1_0.dll "$(DLL)"
	@echo "loader: $(DLL)"

check:
	@if [ -n "$(NMS_EXE)" ]; then python3 tools/check_specs.py "$(NMS_EXE)" loader/mods; \
	else python3 tools/check_specs.py; fi

dist: loader
	@rm -rf "$(BUILD)/fwzip" && mkdir -p "$(BUILD)/fwzip/Binaries"
	@cp "$(DLL)" "$(BUILD)/fwzip/Binaries/"
	@mkdir -p "$(DIST)" && rm -f "$(FRAMEWORK_ZIP)" && cd "$(BUILD)/fwzip" && zip -qr "$(shell pwd)/$(FRAMEWORK_ZIP)" Binaries
	@echo "packed: $(FRAMEWORK_ZIP)"
	@unzip -l "$(FRAMEWORK_ZIP)" | tail -3

clean:
	@rm -rf loader/target loader/dist build dist

assets:
	python3 assets/generate.py
