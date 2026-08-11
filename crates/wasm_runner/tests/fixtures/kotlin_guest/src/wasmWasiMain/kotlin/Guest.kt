// A fixture Kotlin guest module implementing the Convex WASM ABI, used by
// the wasm_runner integration tests (tests/kotlin_guest_e2e.rs).
//
// Target: Kotlin Multiplatform `wasmWasi` (wasm32-wasip1 + WasmGC, no JS
// runtime). The host functions are imported under module `env` and match
// crates/wasm_runner/src/abi.rs exactly. The runner registers WASI preview1
// (deterministic clocks/RNG) and calls `_initialize` when present; the
// reactor form emitted here (no `main`) instead runs its initializers in the
// Wasm start section, so no extra host calls are needed.
//
// There is deliberately no `fun main()`: Kotlin then emits a reactor module
// (start section + `@WasmExport` functions + `memory`, no `_start`), which
// is the shape this host expects. Built with:
//
//   gradle build        # -> build/bin/wasmWasi/**/kotlin_guest.wasm
//
// (Requires a JDK 11+ and Gradle 8.x; see README.md.)

@file:OptIn(ExperimentalWasmInterop::class, UnsafeWasmMemoryApi::class)

package guest

import kotlin.wasm.ExperimentalWasmInterop
import kotlin.wasm.WasmExport
import kotlin.wasm.WasmImport
import kotlin.wasm.unsafe.MemoryAllocator
import kotlin.wasm.unsafe.Pointer
import kotlin.wasm.unsafe.UnsafeWasmMemoryApi
import kotlin.wasm.unsafe.withScopedMemoryAllocator

// The host sends `{"function": "<name>", "args": [...]}` (see wasm_runner
// src/lib.rs); keep these in sync with the C guest fixture.
private const val INPUT_CAPACITY = 65536
private const val OUTPUT_CAPACITY = 4096

// ---------------------------------------------------------------------------
// Host functions (module "env"), matching wasm_runner/src/abi.rs.
// @WasmImport passes raw wasm types through without adapters (Int -> i32).
// ---------------------------------------------------------------------------

@WasmImport("env", "__convex_input_length")
private external fun convexInputLength(): Int

@WasmImport("env", "__convex_input_load")
private external fun convexInputLoad(offset: Int, dest: Int, length: Int)

@WasmImport("env", "__convex_output_set")
private external fun convexOutputSet(ptr: Int, length: Int)

@WasmImport("env", "__convex_error_set")
private external fun convexErrorSet(ptr: Int, length: Int)

@WasmImport("env", "__convex_log")
private external fun convexLog(ptr: Int, length: Int)

// ---------------------------------------------------------------------------
// Linear-memory helpers (kotlin.wasm.unsafe).
// ---------------------------------------------------------------------------

// Byte search: find `needle` inside `haystack` (linear memory), or -1.
private fun findSubstring(haystack: Pointer, haystackLen: Int, needle: String): Int {
    val needleBytes = needle.encodeToByteArray()
    if (needleBytes.isEmpty() || needleBytes.size > haystackLen) return -1
    val limit = haystackLen - needleBytes.size
    outer@ for (i in 0..limit) {
        for (j in needleBytes.indices) {
            if (haystack.plus(i + j).loadByte() != needleBytes[j]) continue@outer
        }
        return i
    }
    return -1
}

// Copy a Kotlin string's UTF-8 bytes into linear memory. Returns bytes written.
private fun storeAscii(dst: Pointer, s: String): Int {
    val bytes = s.encodeToByteArray()
    for (i in bytes.indices) {
        dst.plus(i).storeByte(bytes[i])
    }
    return bytes.size
}

// Emit the first JSON string token of `buf` (between the first two quotes)
// into `out` as a JSON string literal (with surrounding quotes), returning
// the emitted length, or -1. Plain strings only, no escape sequences, like
// the C guest.
private fun extractFirstString(
    buf: Pointer,
    bufLen: Int,
    out: Pointer,
    outCapacity: Int,
): Int {
    val quote = '"'.code.toByte()
    var start = 0
    while (start < bufLen && buf.plus(start).loadByte() != quote) start++
    if (start >= bufLen) return -1
    start++
    var end = start
    while (end < bufLen && buf.plus(end).loadByte() != quote) end++
    if (end >= bufLen) return -1
    val len = end - start
    if (len > outCapacity - 2) return -1
    out.storeByte(quote)
    for (i in 0 until len) {
        out.plus(i + 1).storeByte(buf.plus(start + i).loadByte())
    }
    out.plus(len + 1).storeByte(quote)
    return len + 2
}

// Report an error to the host. The host copies the message out of linear
// memory synchronously, so the scoped arena may be freed afterwards.
private fun reportError(allocator: MemoryAllocator, message: String) {
    val bytes = message.encodeToByteArray()
    val buf = allocator.allocate(bytes.size)
    for (i in bytes.indices) {
        buf.plus(i).storeByte(bytes[i])
    }
    convexErrorSet(buf.address.toInt(), bytes.size)
}

// ---------------------------------------------------------------------------
// The dispatcher. Returns 0 on success, non-zero on error.
// ---------------------------------------------------------------------------

@WasmExport("__convex_run")
fun convexRun(): Int = withScopedMemoryAllocator { allocator ->
    val inputLen = convexInputLength()
    if (inputLen <= 0 || inputLen > INPUT_CAPACITY) {
        reportError(allocator, "input too large")
        return@withScopedMemoryAllocator 1
    }
    val input = allocator.allocate(inputLen)
    // The host writes the payload into our linear memory (host-alloc ABI).
    convexInputLoad(0, input.address.toInt(), inputLen)

    // Match the exact `"function": "echo"` byte sequence.
    if (findSubstring(input, inputLen, "\"function\": \"echo\"") >= 0) {
        // echo: return the first argument, which is a JSON string.
        val argsIdx = findSubstring(input, inputLen, "\"args\": [")
        val out = allocator.allocate(OUTPUT_CAPACITY)
        val outLen = if (argsIdx < 0) {
            -1
        } else {
            extractFirstString(
                input.plus(argsIdx + 9),
                inputLen - argsIdx - 9,
                out,
                OUTPUT_CAPACITY,
            )
        }
        if (outLen < 0) {
            reportError(allocator, "expected a string argument")
            return@withScopedMemoryAllocator 1
        }
        convexOutputSet(out.address.toInt(), outLen)
        return@withScopedMemoryAllocator 0
    }

    reportError(allocator, "unknown function")
    return@withScopedMemoryAllocator 1
}

// The list of functions in the module, returned as JSON.
@WasmExport("__convex_functions")
fun convexFunctions(): Int = withScopedMemoryAllocator { allocator ->
    val json = """[{"name":"echo","type":"query"}]"""
    val bytes = json.encodeToByteArray()
    val buf = allocator.allocate(bytes.size)
    for (i in bytes.indices) {
        buf.plus(i).storeByte(bytes[i])
    }
    convexOutputSet(buf.address.toInt(), bytes.size)
    0
}
