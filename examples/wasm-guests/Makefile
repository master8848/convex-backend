# Builds every Convex WASM guest example into build/<lang>/*.wasm.
# `make check` verifies toolchains and explains how to install what's missing.

BUILD := build

.PHONY: all rust go c cpp zig check clean
all: rust go c cpp zig

rust:
	@mkdir -p $(BUILD)/rust
	@echo ">> building rust example (requires cargo + wasm32-wasip1 target)"
	@cargo build --manifest-path rust/Cargo.toml --target wasm32-wasip1 --release --quiet
	@cp rust/target/wasm32-wasip1/release/wasm_guest_example.wasm $(BUILD)/rust/
	@echo "   ok: $(BUILD)/rust/wasm_guest_example.wasm"

go:
	@mkdir -p $(BUILD)/go
	@echo ">> building go example (requires Go >= 1.24)"
	@cd go && GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -o wasm_guest_example.wasm .
	@mv go/wasm_guest_example.wasm $(BUILD)/go/
	@echo "   ok: $(BUILD)/go/wasm_guest_example.wasm"

c:
	@mkdir -p $(BUILD)/c
	@echo ">> building c example (requires LLVM clang with wasm32-wasip1; Apple clang lacks it)"
	@clang --target=wasm32-wasip1 -O3 -nostdlib -fno-exceptions -Wl,--no-entry \
		-Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined \
		-o $(BUILD)/c/wasm_guest_example.wasm c/guest.c
	@echo "   ok: $(BUILD)/c/wasm_guest_example.wasm"

cpp:
	@mkdir -p $(BUILD)/cpp
	@echo ">> building cpp example (requires LLVM clang++ with wasm32-wasip1; Apple clang lacks it)"
	@clang++ --target=wasm32-wasip1 -O3 -nostdlib -fno-exceptions -fno-rtti -fno-threadsafe-statics \
		-Wl,--no-entry -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined \
		-o $(BUILD)/cpp/wasm_guest_example.wasm cpp/guest.cpp
	@echo "   ok: $(BUILD)/cpp/wasm_guest_example.wasm"

zig:
	@mkdir -p $(BUILD)/zig
	@echo ">> building zig example (requires zig >= 0.16)"
	@cd zig && zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor -O ReleaseSmall -fstrip \
		--export=__convex_run --export=__convex_functions --name wasm_guest_example
	@mv zig/wasm_guest_example.wasm $(BUILD)/zig/
	@echo "   ok: $(BUILD)/zig/wasm_guest_example.wasm"

# Report which toolchains are present and how to install the missing ones.
check:
	@echo "--- toolchain check ---"
	@command -v cargo >/dev/null && echo "cargo:       ok" || echo "cargo:       MISSING (install rustup: https://rustup.rs)"
	@cargo -v 2>/dev/null | grep -q wasm32-wasip1 || (rustup target list --installed 2>/dev/null | grep -q wasm32-wasip1 \
		&& echo "wasm32-wasip1: ok" || echo "wasm32-wasip1: MISSING (run: rustup target add wasm32-wasip1)")
	@command -v go >/dev/null && echo "go:          ok ($$(go version | cut -d' ' -f3))" || echo "go:          MISSING (https://go.dev/dl)"
	@if clang --target=wasm32-wasip1 -### -x c /dev/null 2>&1 | grep -q -- '--target=wasm32-wasip1'; then \
		echo "clang(wasm): ok"; else echo "clang(wasm): MISSING (install LLVM clang, e.g. 'brew install llvm' — Apple clang lacks wasm targets)"; fi
	@command -v dart >/dev/null && echo "dart:        ok (wasm GC guest: in progress)" || echo "dart:        not installed (needed for dart example once WasmGC lands)"
	@command -v kotlinc >/dev/null && echo "kotlinc:     ok (wasmWasi guest: in progress)" || echo "kotlinc:     not installed (needed for kotlin example)"
	@command -v zig >/dev/null && echo "zig:         ok ($$(zig version))" || echo "zig:         MISSING (install zig: https://ziglang.org/download)")

clean:
	@rm -rf $(BUILD)
	@rm -rf rust/target go/wasm_guest_example.wasm zig/wasm_guest_example.wasm
	@echo "cleaned"
