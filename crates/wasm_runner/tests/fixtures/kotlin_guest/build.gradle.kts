// Minimal Kotlin Multiplatform build producing a wasmWasi (WASM Preview 1 +
// WasmGC) executable module. The runner (crates/wasm_runner) provides WASI
// preview1 and the `env` host functions the guest imports; wasmtime 47
// enables the three proposals Kotlin emits (function-references, gc,
// exceptions/exnref) by default.
@file:OptIn(org.jetbrains.kotlin.gradle.ExperimentalWasmDsl::class)

import org.jetbrains.kotlin.gradle.ExperimentalWasmDsl

plugins {
    kotlin("multiplatform") version "2.3.0"
}

repositories {
    mavenCentral()
}

kotlin {
    wasmWasi {
        // Produces a standalone .wasm (no `main` -> reactor form: the Wasm
        // start section initializes the runtime at instantiation).
        binaries.executable()
    }
}
