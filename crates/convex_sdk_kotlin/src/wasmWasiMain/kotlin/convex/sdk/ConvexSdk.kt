// Convex Kotlin SDK — wasmWasi (wasm32-wasip1 + WasmGC) — mirrors crates/convex_sdk (Rust).
// Hides the host-alloc ABI (env.__convex_*), provides Context/Db, ConvexValue/Document,
// kotlinx.serialization JSON, and annotation/codegen for @ConvexFunctions/@Query/@Mutation.
//
// Usage (Chat.kt):
//   @ConvexFunctions object Messages {
//     @Query fun list(ctx: Context): List<Document> = ctx.db.query("messages")
//     @Mutation fun send(ctx: Context, body: String, author: String?): String {
//       require(body.isNotBlank())
//       return ctx.db.insert("messages", buildJsonObject { put("body", body); put("author", author ?: "anonymous") })
//     }
//   }
//   @WasmExport("__convex_run") fun __convex_run(): Int = Convex.run(registry)
//   @WasmExport("__convex_functions") fun __convex_functions(): Int = Convex.functions(registry)
//
// Validator JSON (args/returns) is carried in FunctionDescriptor.args/returns and emitted
// via __convex_functions as [{name,type,args,returns}] — see crates/wasm_runner/src/engine.rs:WasmFunctionDescriptor.

@file:OptIn(ExperimentalWasmInterop::class, UnsafeWasmMemoryApi::class)

package convex.sdk

import kotlin.wasm.ExperimentalWasmInterop
import kotlin.wasm.WasmExport
import kotlin.wasm.WasmImport
import kotlin.wasm.unsafe.MemoryAllocator
import kotlin.wasm.unsafe.Pointer
import kotlin.wasm.unsafe.UnsafeWasmMemoryApi
import kotlin.wasm.unsafe.withScopedMemoryAllocator
import kotlinx.serialization.json.*

// ---------------------------------------------------------------------------
// Host imports (module "env"), matching crates/wasm_runner/src/abi.rs.
// ---------------------------------------------------------------------------

@WasmImport("env", "__convex_input_length")
private external fun convexInputLength(): Int

@WasmImport("env", "__convex_input_load")
private external fun convexInputLoad(offset: Int, dest: Int, length: Int)

@WasmImport("env", "__convex_call_data_load")
private external fun convexCallDataLoad(offset: Int, dest: Int, length: Int)

@WasmImport("env", "__convex_output_set")
private external fun convexOutputSet(ptr: Int, length: Int)

@WasmImport("env", "__convex_error_set")
private external fun convexErrorSet(ptr: Int, length: Int)

@WasmImport("env", "__convex_log")
private external fun convexLog(ptr: Int, length: Int)

@WasmImport("env", "__convex_now_ms")
private external fun convexNowMs(): Long

@WasmImport("env", "__convex_random_bytes")
private external fun convexRandomBytes(dest: Int, length: Int)

@WasmImport("env", "__convex_db_get")
private external fun convexDbGet(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_insert")
private external fun convexDbInsert(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_replace")
private external fun convexDbReplace(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_patch")
private external fun convexDbPatch(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_delete")
private external fun convexDbDelete(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_count")
private external fun convexDbCount(ptr: Int, length: Int): Long

@WasmImport("env", "__convex_db_query")
private external fun convexDbQuery(ptr: Int, length: Int): Long

// ---------------------------------------------------------------------------
// Linear-memory helpers (internal).
// ---------------------------------------------------------------------------

private fun storeBytes(dst: Pointer, bytes: ByteArray) {
    for (i in bytes.indices) dst.plus(i).storeByte(bytes[i])
}

private fun loadBytes(src: Pointer, len: Int): ByteArray {
    return ByteArray(len) { src.plus(it).loadByte() }
}

private fun emitJsonString(allocator: MemoryAllocator, json: String, setter: (Int, Int) -> Unit) {
    val bytes = json.encodeToByteArray()
    if (bytes.isEmpty()) {
        // Host expects no-op on empty; still allocate 1 to avoid null pointer.
        val buf = allocator.allocate(1)
        setter(buf.address.toInt(), 0)
        return
    }
    val buf = allocator.allocate(bytes.size)
    storeBytes(buf, bytes)
    setter(buf.address.toInt(), bytes.size)
}

private fun readInputBytes(allocator: MemoryAllocator): ByteArray? {
    val len = convexInputLength()
    if (len <= 0) return ByteArray(0)
    if (len > 65536) {
        val msg = "input too large".encodeToByteArray()
        val buf = allocator.allocate(msg.size)
        storeBytes(buf, msg)
        convexErrorSet(buf.address.toInt(), msg.size)
        return null
    }
    val ptr = allocator.allocate(len)
    convexInputLoad(0, ptr.address.toInt(), len)
    return loadBytes(ptr, len)
}

private fun readCallData(offset: Int, length: Int, allocator: MemoryAllocator): ByteArray {
    if (length == 0) return ByteArray(0)
    val dst = allocator.allocate(length)
    convexCallDataLoad(offset, dst.address.toInt(), length)
    return loadBytes(dst, length)
}

// ---------------------------------------------------------------------------
// Public types: ConvexValue, Document, ConvexError, Context, Database.
// ---------------------------------------------------------------------------

/**
 * A Convex value — superset of JSON. In Kotlin we alias to JsonElement and
 * provide helpers to build from JSON. Mirrors Rust's ConvexValue.
 */
typealias ConvexValue = JsonElement

fun ConvexValue.Companion_fromJson(json: String): ConvexValue = Json.parseToJsonElement(json)

data class Document(val value: JsonObject) {
    val id: String get() = value["_id"]?.jsonPrimitive?.content ?: ""
    val creationTime: Long get() = value["_creationTime"]?.jsonPrimitive?.longOrNull ?: 0L
    companion object {
        fun fromJson(obj: JsonObject): Document = Document(obj)
        fun fromJsonElement(e: JsonElement): Document = Document(e.jsonObject)
    }
    fun toJsonElement(): JsonElement = value
}

class ConvexError(message: String, val code: String? = null) : Exception(message)

class Context(val db: Database = Database()) {
    fun log(message: String) {
        withScopedMemoryAllocator { allocator ->
            val bytes = message.encodeToByteArray()
            if (bytes.isNotEmpty()) {
                val buf = allocator.allocate(bytes.size)
                storeBytes(buf, bytes)
                convexLog(buf.address.toInt(), bytes.size)
            }
        }
    }
    fun now(): Long = convexNowMs()
    fun randomBytes(len: Int): ByteArray = withScopedMemoryAllocator { allocator ->
        val buf = allocator.allocate(len)
        convexRandomBytes(buf.address.toInt(), len)
        loadBytes(buf, len)
    }
    fun random(): ByteArray = randomBytes(32)
}

class Database {
    private fun dbCall(op: String, args: JsonElement): JsonElement = withScopedMemoryAllocator { allocator ->
        val argBytes = Json.encodeToString(JsonElement.serializer(), args).encodeToByteArray()
        val result: Long = if (argBytes.isEmpty()) {
            // Should not happen; args is always an object.
            when (op) {
                "__convex_db_get" -> convexDbGet(0, 0)
                "__convex_db_insert" -> convexDbInsert(0, 0)
                "__convex_db_replace" -> convexDbReplace(0, 0)
                "__convex_db_patch" -> convexDbPatch(0, 0)
                "__convex_db_delete" -> convexDbDelete(0, 0)
                "__convex_db_count" -> convexDbCount(0, 0)
                "__convex_db_query" -> convexDbQuery(0, 0)
                else -> -1
            }
        } else {
            val ptr = allocator.allocate(argBytes.size)
            storeBytes(ptr, argBytes)
            val p = ptr.address.toInt()
            val l = argBytes.size
            when (op) {
                "__convex_db_get" -> convexDbGet(p, l)
                "__convex_db_insert" -> convexDbInsert(p, l)
                "__convex_db_replace" -> convexDbReplace(p, l)
                "__convex_db_patch" -> convexDbPatch(p, l)
                "__convex_db_delete" -> convexDbDelete(p, l)
                "__convex_db_count" -> convexDbCount(p, l)
                "__convex_db_query" -> convexDbQuery(p, l)
                else -> -1
            }
        }
        if (result < 0) throw ConvexError("Database operation failed")
        val offset = (result shr 32).toInt()
        val len = result.toInt()
        val data = readCallData(offset, len, allocator)
        val envelopeStr = data.decodeToString()
        val envelope = Json.parseToJsonElement(envelopeStr).jsonObject
        if (envelope.containsKey("ok")) {
            envelope["ok"]!!
        } else {
            val msg = envelope["err"]?.jsonPrimitive?.content ?: "Database error"
            throw ConvexError(msg)
        }
    }

    fun query(table: String): List<Document> {
        val result = dbCall("__convex_db_query", buildJsonObject { put("table", table) })
        val arr = result.jsonArray
        return arr.map { Document.fromJsonElement(it) }
    }

    fun count(table: String): Long {
        val result = dbCall("__convex_db_count", buildJsonObject { put("table", table) })
        return result.jsonPrimitive.longOrNull ?: result.jsonPrimitive.double.toLong()
    }

    fun get(table: String, id: String): Document? {
        val result = dbCall("__convex_db_get", buildJsonObject { put("table", table); put("id", id) })
        if (result is JsonNull) return null
        return Document.fromJsonElement(result)
    }

    fun insert(table: String, value: JsonElement): String {
        val result = dbCall("__convex_db_insert", buildJsonObject { put("table", table); put("value", value) })
        // result is the inserted document; extract _id
        val obj = result.jsonObject
        return obj["_id"]?.jsonPrimitive?.content ?: result.jsonPrimitive.content
    }

    fun insert(table: String, value: Map<String, String>): String =
        insert(table, buildJsonObject { for ((k, v) in value) put(k, v) })

    fun patch(table: String, id: String, value: JsonElement): Document {
        val result = dbCall("__convex_db_patch", buildJsonObject { put("table", table); put("id", id); put("value", value) })
        return Document.fromJsonElement(result)
    }

    fun replace(table: String, id: String, value: JsonElement): Document {
        val result = dbCall("__convex_db_replace", buildJsonObject { put("table", table); put("id", id); put("value", value) })
        return Document.fromJsonElement(result)
    }

    fun delete(table: String, id: String): Document {
        val result = dbCall("__convex_db_delete", buildJsonObject { put("table", table); put("id", id) })
        return Document.fromJsonElement(result)
    }
}

// ---------------------------------------------------------------------------
// Annotations (markers) — mirrors Rust's #[query]/#[mutation]/#[convex_functions].
// ---------------------------------------------------------------------------

@Target(AnnotationTarget.CLASS, AnnotationTarget.OBJECT)
annotation class ConvexFunctions

@Target(AnnotationTarget.FUNCTION)
annotation class Query

@Target(AnnotationTarget.FUNCTION)
annotation class Mutation

@Target(AnnotationTarget.FUNCTION)
annotation class Action

// ---------------------------------------------------------------------------
// FunctionDescriptor + Registry (dispatcher + __convex_functions).
// ---------------------------------------------------------------------------

data class FunctionDescriptor(
    val name: String,
    val type: String, // "query" | "mutation" | "action" | "httpAction"
    val args: String? = null,
    val returns: String? = null,
    val visibility: String? = null
)

class ConvexRegistry {
    private val handlers = mutableMapOf<String, Pair<FunctionDescriptor, (Context, JsonArray) -> JsonElement>>()

    fun query(name: String, args: String? = null, returns: String? = null, handler: (Context, JsonArray) -> JsonElement) {
        handlers[name] = FunctionDescriptor(name, "query", args, returns) to handler
    }
    fun mutation(name: String, args: String? = null, returns: String? = null, handler: (Context, JsonArray) -> JsonElement) {
        handlers[name] = FunctionDescriptor(name, "mutation", args, returns) to handler
    }
    fun action(name: String, args: String? = null, returns: String? = null, handler: (Context, JsonArray) -> JsonElement) {
        handlers[name] = FunctionDescriptor(name, "action", args, returns) to handler
    }

    // Typed helpers that auto-serialize return values via kotlinx.serialization.
    // Usage: queryTyped("list") { ctx -> ctx.db.query("messages").map { it.value } }
    // For now, typed helpers are convenience wrappers around JsonArray handlers.

    fun run(): Int = withScopedMemoryAllocator { allocator ->
        val inputBytes = readInputBytes(allocator) ?: return@withScopedMemoryAllocator 1
        val inputStr = inputBytes.decodeToString()
        val input = try { Json.parseToJsonElement(inputStr).jsonObject } catch (e: Exception) {
            emitJsonString(allocator, e.message ?: "Invalid input payload") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        }
        val name = input["function"]?.jsonPrimitive?.content
        if (name == null) {
            emitJsonString(allocator, "Input payload is missing the \"function\" field") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        }
        val argsElement = input["args"]
        val args: JsonArray = when {
            argsElement is JsonArray -> argsElement
            argsElement == null -> JsonArray(emptyList())
            else -> {
                emitJsonString(allocator, "Input payload's \"args\" field must be an array") { p, l -> convexErrorSet(p, l) }
                return@withScopedMemoryAllocator 1
            }
        }
        val entry = handlers[name]
        if (entry == null) {
            emitJsonString(allocator, "Function $name not found in this module") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        }
        val (_, handler) = entry
        val ctx = Context()
        val result: JsonElement = try {
            handler(ctx, args)
        } catch (e: ConvexError) {
            emitJsonString(allocator, e.message ?: "ConvexError") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        } catch (e: IllegalArgumentException) {
            emitJsonString(allocator, e.message ?: "Invalid argument") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        } catch (e: Exception) {
            emitJsonString(allocator, e.message ?: "Internal error") { p, l -> convexErrorSet(p, l) }
            return@withScopedMemoryAllocator 1
        }
        val outStr = Json.encodeToString(JsonElement.serializer(), result)
        emitJsonString(allocator, outStr) { p, l -> convexOutputSet(p, l) }
        0
    }

    fun functions(): Int = withScopedMemoryAllocator { allocator ->
        val descriptors = handlers.values.map { (d, _) ->
            buildJsonObject {
                put("name", d.name)
                put("type", d.type)
                if (d.args != null) put("args", d.args) else put("args", JsonNull)
                if (d.returns != null) put("returns", d.returns) else put("returns", JsonNull)
                if (d.visibility != null) put("visibility", d.visibility)
            }
        }
        // Emit as JSON array; args/returns are strings (validator JSON) or null — match WasmFunctionDescriptor {args: Option<String>}
        // Older guests emitted only [{name,type}]; extended descriptor keeps compat via defaults.
        // To match current validation.rs which accepts Option<String>, we emit strings when present, otherwise omit.
        val out = JsonArray(descriptors.map { obj ->
            // If args/returns are JsonNull, strip to keep wire compatible with older parsers that expect missing
            // — but engine accepts null/default, so we keep explicit null for clarity; also support string form.
            // For minimal wire, emit only name/type when args/returns are null (like Rust SDK).
            val filtered = buildJsonObject {
                put("name", obj["name"]!!)
                put("type", obj["type"]!!)
                val a = obj["args"]
                if (a != null && a !is JsonNull) put("args", a)
                val r = obj["returns"]
                if (r != null && r !is JsonNull) put("returns", r)
                val v = obj["visibility"]
                if (v != null && v !is JsonNull) put("visibility", v)
            }
            filtered
        })
        val outStr = Json.encodeToString(JsonElement.serializer(), out)
        emitJsonString(allocator, outStr) { p, l -> convexOutputSet(p, l) }
        0
    }

    // For testing: expose descriptor list as typed objects
    fun descriptorList(): List<FunctionDescriptor> = handlers.values.map { it.first }
}

fun convexRegistry(block: ConvexRegistry.() -> Unit): ConvexRegistry = ConvexRegistry().apply(block)

// Convenience top-level helpers for generated exports:
//   @WasmExport("__convex_run") fun __convex_run(): Int = registry.run()
//   @WasmExport("__convex_functions") fun __convex_functions(): Int = registry.functions()
