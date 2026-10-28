# Nozavolochen framework
#
#   make loader     cross-build xinput9_1_0.dll (mingw, Linux -> Windows)
#   make dist       pack the framework release zip
#   make assets     regenerate the Nexus page images (header + main)
#   make lang-check verify loader/strings/*.json (keys, lengths, codes)
#   make clean      drop build outputs
#
# Spec verification lives with the mods: each mod runs
# tools/check_specs.py against its own folder
# (see the mod Makefiles). The framework ships no mods,
# so there is nothing to verify here.
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

.PHONY: loader dist assets clean lang-check

loader:
	@cd loader && cargo build --target x86_64-pc-windows-gnu --release
	@mkdir -p "$(LOADER_DIST)"
	@cp loader/target/x86_64-pc-windows-gnu/release/xinput9_1_0.dll "$(DLL)"
	@echo "loader: $(DLL)"

dist: loader
	@rm -rf "$(BUILD)/fwzip" && mkdir -p "$(BUILD)/fwzip"
	@cp "$(DLL)" "$(BUILD)/fwzip/"
	@mkdir -p "$(DIST)" && rm -f "$(FRAMEWORK_ZIP)" && cd "$(BUILD)/fwzip" && zip -qr "$(shell pwd)/$(FRAMEWORK_ZIP)" xinput9_1_0.dll
	@echo "packed: $(FRAMEWORK_ZIP)"
	@unzip -l "$(FRAMEWORK_ZIP)" | tail -3

clean:
	@rm -rf loader/target loader/dist build dist

assets:
	python3 assets/generate.py

lang-check:
	python3 tools/check_strings.py
