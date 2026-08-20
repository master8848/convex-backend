// Convex Kotlin SDK — wasmWasi library (WasmGC) — reusable across guests.
// This is the canonical home for ConvexSdk.kt; guests vendor it or consume via
// composite build (includeBuild). For now fixtures and demos vendor the file
// directly (no Maven publish) to keep `gradle build` hermetic without extra
// publication steps. This build validates the SDK itself compiles as a library.
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
        // SDK as library (not executable) — fixtures/demos produce the executable.
        // Using `binaries.library()` keeps the SDK consumable as `project(":convex_sdk_kotlin")`.
        // For now we just ensure the sources compile; the SDK file is vendored.
    }
    sourceSets {
        val wasmWasiMain by getting {
            dependencies {
                implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.8.1")
            }
        }
    }
}
