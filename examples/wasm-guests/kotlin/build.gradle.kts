// Kotlin guest example — ergonomic SDK (convex-kotlin-sdk).
// Mirrors crates/wasm_runner/tests/fixtures/kotlin_guest but standalone.
// Target: wasm32-wasip1 + WasmGC (Kotlin 2.3.0), reactor module (no `main`).
@file:OptIn(org.jetbrains.kotlin.gradle.ExperimentalWasmDsl::class)

import org.jetbrains.kotlin.gradle.ExperimentalWasmDsl

plugins {
    kotlin("multiplatform") version "2.3.0"
    kotlin("plugin.serialization") version "2.3.0"
}

repositories {
    mavenCentral()
}

kotlin {
    wasmWasi {
        binaries.executable()
    }
    sourceSets {
        val wasmWasiMain by getting {
            dependencies {
                implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.8.1")
            }
        }
    }
}

tasks.register<Copy>("copyWasm") {
    dependsOn("build")
    from(layout.buildDirectory.dir("bin/wasmWasi")) {
        include("**/*.wasm")
    }
    into(layout.projectDirectory.dir("build"))
}
