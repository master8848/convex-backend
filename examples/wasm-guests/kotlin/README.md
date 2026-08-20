# Kotlin guest — valid target (wasmWasi + WasmGC)

Ergonomic Kotlin guest via `convex-kotlin-sdk` (`crates/convex_sdk_kotlin/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt`),
mirroring the Rust `convex_sdk` experience. See `crates/wasm_runner/tests/fixtures/kotlin_guest/src/wasmWasiMain/kotlin/Guest.kt`
for queries, mutations, database reads/writes, logs, and validator JSON.

## Usage

```kotlin
@ConvexFunctions object Messages {
    @Query fun list(ctx: Context): List<Document> = ctx.db.query("messages")
    @Mutation fun send(ctx: Context, body: String, author: String?): String {
        require(body.isNotBlank())
        return ctx.db.insert("messages", buildJsonObject { put("body", body); put("author", author ?: "anonymous") })
    }
}
private val registry = convexRegistry {
    query("list") { ctx, _ -> JsonArray(Messages.list(ctx).map { it.value }) }
    mutation("send") { ctx, args -> /* deserialize args, call Messages.send */ }
}
@WasmExport("__convex_run") fun convexRun(): Int = registry.run()
@WasmExport("__convex_functions") fun convexFunctions(): Int = registry.functions()
```

No manual `Pointer`, `findSubstring`, or `withScopedMemoryAllocator` — the SDK hides the host-alloc ABI
(`env.__convex_*`, `__convex_call_data_load`, `__convex_output_set`) and uses `kotlinx.serialization` for JSON.

`__convex_functions` emits `[{"name":"list","type":"query","args":...,"returns":...}]` with validator JSON
(args/returns) — see `crates/wasm_runner/src/engine.rs:WasmFunctionDescriptor`.

## Build

Requires JDK 11+ and Gradle 8.x (same as the fixture). From the fixture dir:

```sh
cd crates/wasm_runner/tests/fixtures/kotlin_guest
gradle build --console=plain
find build -name '*.wasm'  # -> build/bin/wasmWasi/debugExecutable/kotlin_guest.wasm
```

Or for the demo chat guests:

```sh
cd demos/demo-chat-kotlin/convex/kotlin   # or demos/demo-chat-polyglot/convex/kotlin
gradle build --console=plain
gradle copyWasm  # -> ../demo_chat_kotlin.wasm (or ../analytics.wasm)
```

Single-command parity with Rust (`cargo build --target wasm32-wasip1`): `gradle build` + `gradle copyWasm`
copies to `convex/*.wasm` for `convex dev`, matching `examples/wasm-guests/scaffold.sh` patterns.

## Verify

```sh
wasm-tools print build/bin/wasmWasi/debugExecutable/kotlin_guest.wasm | head -50
# imports only from wasi_snapshot_preview1 and env (__convex_*); exports memory, __convex_run, __convex_functions
```

## Scaffold

```sh
./scaffold.sh kotlin my_guest   # (coming soon — for now copy examples/wasm-guests/kotlin or crates/wasm_runner/tests/fixtures/kotlin_guest)
```

## Status

✅ **valid target** — Kotlin Multiplatform `wasmWasi` (wasm32-wasip1 + WasmGC). The runner (wasmtime 47) enables
`function-references`, `gc`, and `exceptions` (new `exnref`) by default, so no engine config is needed.
Reactor module (no `main`), WASI preview1 + `env` host functions only.

See `docs/wasm.md`, `docs/kotlin-guest.md`, and `crates/wasm_runner/tests/fixtures/kotlin_guest/README.md`.
