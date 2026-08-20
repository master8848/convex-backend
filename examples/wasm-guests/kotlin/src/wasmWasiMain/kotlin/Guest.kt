@file:OptIn(kotlin.wasm.ExperimentalWasmInterop::class)
package guest
import convex.sdk.*
import kotlinx.serialization.json.*
import kotlin.wasm.WasmExport; import kotlin.wasm.ExperimentalWasmInterop

@ConvexFunctions object GuestFns {
    @Query fun echo(ctx: Context, value: String): String { ctx.log("echo called with $value"); return value }
    @Query fun list(ctx: Context): List<Document> = ctx.db.query("messages")
    @Mutation fun send(ctx: Context, body: String, author: String?): String {
        require(body.isNotBlank()); val a=author?.takeIf{it.isNotBlank()}?:"anonymous"
        return ctx.db.insert("messages", buildJsonObject{put("body",body); put("author",a)})
    }
}
private val registry = convexRegistry {
    query("echo", args="""{"type":"object","value":{"value":{"fieldType":{"type":"string"},"optional":false}}}""") { ctx,args -> Json.encodeToJsonElement(GuestFns.echo(ctx,args[0].jsonPrimitive.content)) }
    query("list", args="""{"type":"object","value":{}}""") { ctx,_ -> JsonArray(GuestFns.list(ctx).map{it.value}) }
    mutation("send", args="""{"type":"object","value":{"body":{"fieldType":{"type":"string"},"optional":false},"author":{"fieldType":{"type":"string"},"optional":true}}}""") { ctx,args ->
        val o=args[0].jsonObject; Json.encodeToJsonElement(GuestFns.send(ctx,o["body"]!!.jsonPrimitive.content, o["author"]?.let{if(it is JsonNull) null else it.jsonPrimitive.content}))
    }
}
@WasmExport("__convex_run") fun convexRun(): Int = registry.run()
@WasmExport("__convex_functions") fun convexFunctions(): Int = registry.functions()
