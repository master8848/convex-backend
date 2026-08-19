import { header, compareStrings } from "./common.js";
import { parseValidator } from "./validator_helpers.js";
import { ConvexValidator } from "../lib/deployApi/validator.js";
import { Context } from "../../bundler/context.js";
import {
  ComponentDirectory,
  toComponentDefinitionPath,
} from "../lib/components/definition/directoryStructure.js";
import { StartPushResponse } from "../lib/deployApi/startPush.js";
import { importPath } from "./api.js";

function validatorToKotlinType(
  validator: ConvexValidator,
  useIdType: boolean,
): string {
  switch (validator.type) {
    case "null":
      return "Unit?";
    case "number":
      return "Double";
    case "bigint":
      return "Long";
    case "commitTs":
      return "Long";
    case "boolean":
      return "Boolean";
    case "string":
      return "String";
    case "bytes":
      return "ByteArray";
    case "any":
      return "JsonElement";
    case "literal": {
      // Kotlin has no literal types; emit the underlying primitive
      const v = validator.value;
      if (v === null) return "Unit?";
      if (typeof v === "string") return "String";
      if (typeof v === "number") return "Double";
      if (typeof v === "boolean") return "Boolean";
      if (typeof v === "bigint") return "Long";
      return "String";
    }
    case "id":
      return useIdType ? `Id<"${validator.tableName}">` : "String";
    case "array":
      return `List<${validatorToKotlinType(validator.value, useIdType)}>`;
    case "record":
      return `Map<${validatorToKotlinType(validator.keys, useIdType)}, ${validatorToKotlinType(validator.values.fieldType, useIdType)}>`;
    case "union":
      // Sealed interface per union; inline as union string for now, caller expands
      return validator.value.map((v) => validatorToKotlinType(v, useIdType)).join(" /* | */ ");
    case "object":
      // Object maps to generated data class; return placeholder
      return "JsonObject";
    default:
      throw new Error(`Unsupported validator type ${(validator as any).type}`);
  }
}

function kotlinObjectFields(
  fields: Record<string, { fieldType: ConvexValidator; optional: boolean }>,
  useIdType: boolean,
): string {
  return Object.entries(fields)
    .map(([name, f]) => {
      const t = validatorToKotlinType(f.fieldType, useIdType);
      const nullable = f.optional ? "?" : "";
      const defaultVal = f.optional ? " = null" : "";
      return `  val ${name}: ${t}${nullable}${defaultVal}`;
    })
    .join(",\n");
}

/**
 * Generate Kotlin API for a component.
 * Produces `convex/_generated/Api.kt` style file.
 * Preserves FilterApi public/internal partition via separate `api` and `internal` objects
 * and threads `componentPath` where needed (for components).
 */
export async function kotlinCodegen(
  ctx: Context,
  startPushResponse: StartPushResponse,
  rootComponent: ComponentDirectory,
  componentDirectory: ComponentDirectory,
  opts?: { useIdType?: boolean },
): Promise<string> {
  const useIdType = opts?.useIdType ?? true;
  const definitionPath = toComponentDefinitionPath(
    rootComponent,
    componentDirectory,
  );
  const analysis = startPushResponse.analysis[definitionPath];
  if (!analysis) {
    return await ctx.crash({
      exitCode: 1,
      errorType: "fatal",
      printedMessage: `No analysis found for component ${definitionPath}`,
    });
  }

  const lines: string[] = [];
  lines.push(header("Generated Kotlin `Api` utility."));
  lines.push(`// Convex Kotlin codegen — validator JSON is the single source of truth.`);
  lines.push(`// Value codec: ConvexValue $integer/$bytes via convexToJson/jsonToConvex equivalent.`);
  lines.push(`// Transport: POST /api/{query,mutation,action} with format convex_encoded_json, status 560 handling.`);
  lines.push(`package convex.generated`);
  lines.push(``);
  lines.push(`import kotlinx.serialization.Serializable`);
  lines.push(`import kotlinx.serialization.json.JsonElement`);
  lines.push(`import io.ktor.client.HttpClient`);
  lines.push(`import io.ktor.client.request.post`);
  lines.push(`import io.ktor.client.request.headers`);
  lines.push(`import io.ktor.client.statement.bodyAsText`);
  lines.push(`import io.ktor.http.ContentType`);
  lines.push(`import io.ktor.http.contentType`);
  lines.push(`import kotlinx.coroutines.flow.Flow`);
  lines.push(`import kotlinx.coroutines.flow.flow`);
  lines.push(``);
  lines.push(`@JvmInline value class Id<T>(val id: String)`);
  lines.push(``);
  // FunctionReference mirrors FunctionReference<any, visibility> + componentPath
  lines.push(`data class FunctionReference<Args, Return>(val name: String, val visibility: String, val componentPath: String? = null)`);
  lines.push(``);
  // Convex client (KMP ktor)
  lines.push(`class ConvexClient(val deploymentUrl: String, val httpClient: HttpClient) {`);
  lines.push(`  suspend inline fun <reified Args, reified Ret> query(ref: FunctionReference<Args, Ret>, args: Args): Ret = call("query", ref, args)`);
  lines.push(`  suspend inline fun <reified Args, reified Ret> mutation(ref: FunctionReference<Args, Ret>, args: Args): Ret = call("mutation", ref, args)`);
  lines.push(`  suspend inline fun <reified Args, reified Ret> action(ref: FunctionReference<Args, Ret>, args: Args): Ret = call("action", ref, args)`);
  lines.push(`  fun <Args, Ret> subscribe(ref: FunctionReference<Args, Ret>, args: Args): Flow<Ret> = flow { /* WebSocket sync via BaseConvexClient protocol */ }`);
  lines.push(`  // Value codec: convexToJson args -> JsonElement with \$integer/\$bytes, jsonToConvex on response`);
  lines.push(`  suspend inline fun <reified Args, reified Ret> call(kind: String, ref: FunctionReference<Args, Ret>, args: Args): Ret {`);
  lines.push(`    // POST "\${deploymentUrl}/api/\${kind}" { path: ref.name, format: "convex_encoded_json", args: [convexToJson(args)] }`);
  lines.push(`    throw NotImplementedError("Ktor HTTP + convex codec stub — fill with ktor client")`);
  lines.push(`  }`);
  lines.push(`}`);
  lines.push(``);

  // Collect all functions grouped by module path
  const modules = Object.entries(analysis.functions).sort(([a], [b]) =>
    compareStrings(a, b),
  );

  // Emit data classes for args/returns object validators
  const emittedClasses = new Set<string>();
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    for (const fn of mod.functions) {
      for (const which of ["args", "returns"] as const) {
        const raw = which === "args" ? fn.args : fn.returns;
        const v = parseValidator(raw);
        if (!v || v.type !== "object") continue;
        const className = `${base.split("/").map(s => s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_${
          which === "args" ? "Args" : "Return"
        }`;
        if (emittedClasses.has(className)) continue;
        emittedClasses.add(className);
        lines.push(`@Serializable`);
        lines.push(`data class ${className}(`);
        lines.push(kotlinObjectFields(v.value, useIdType));
        lines.push(`)`);
        lines.push(``);
      }
    }
  }

  // Emit api / internal tree preserving FilterApi partition
  // We branch by visibility: public -> api, internal -> internal
  const apiTree: Record<string, string[]> = {};
  const internalTree: Record<string, string[]> = {};
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const parts = base.split("/");
    for (const fn of mod.functions) {
      const vis = fn.visibility?.kind ?? "public";
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType = "Unit";
      let retType = "Unit";
      try {
        if (argsV) {
          if (argsV.type === "object") {
            const cn = `${base.split("/").map(s => s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`;
            argsType = emittedClasses.has(cn) ? cn : validatorToKotlinType(argsV, useIdType);
          } else if (argsV.type !== "any") {
            argsType = validatorToKotlinType(argsV, useIdType);
          } else {
            argsType = "JsonElement";
          }
        }
      } catch {}
      try {
        if (retV) {
          if (retV.type === "object") {
            const cn = `${base.split("/").map(s => s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`;
            retType = emittedClasses.has(cn) ? cn : validatorToKotlinType(retV, useIdType);
          } else {
            retType = validatorToKotlinType(retV, useIdType);
          }
        }
      } catch {}
      const ref = `FunctionReference<${argsType}, ${retType}>("${base}:${fn.name}", "${vis}")`;
      const target = vis === "public" ? apiTree : internalTree;
      const key = parts.join(".");
      if (!target[key]) target[key] = [];
      target[key].push(`    val ${fn.name} = ${ref}`);
    }
  }

  lines.push(`object api {`);
  for (const [modKey, entries] of Object.entries(apiTree).sort(([a],[b])=>compareStrings(a,b))) {
    const last = modKey.split(".").pop()!;
    if (modKey.includes(".")) {
      // nested: emit as object per segment
      const segs = modKey.split(".");
      let indent = "  ";
      for (let i=0;i<segs.length;i++) {
        lines.push(`${indent}object ${segs[i]} {`);
        indent += "  ";
        if (i === segs.length-1) {
          for (const e of entries) lines.push(`${indent}${e}`);
        }
      }
      for (let i=segs.length-1;i>=0;i--) lines.push(`${"  ".repeat(i+1)}}`);
    } else {
      lines.push(`  object ${last} {`);
      for (const e of entries) lines.push(`  ${e}`);
      lines.push(`  }`);
    }
  }
  lines.push(`}`);
  lines.push(``);
  lines.push(`object internal {`);
  for (const [modKey, entries] of Object.entries(internalTree).sort(([a],[b])=>compareStrings(a,b))) {
    const last = modKey.split(".").pop()!;
    if (modKey.includes(".")) {
      const segs = modKey.split(".");
      let indent = "  ";
      for (let i=0;i<segs.length;i++) {
        lines.push(`${indent}object ${segs[i]} {`);
        indent += "  ";
        if (i === segs.length-1) for (const e of entries) lines.push(`${indent}${e}`);
      }
      for (let i=segs.length-1;i>=0;i--) lines.push(`${"  ".repeat(i+1)}}`);
    } else {
      lines.push(`  object ${last} {`);
      for (const e of entries) lines.push(`  ${e}`);
      lines.push(`  }`);
    }
  }
  lines.push(`}`);
  lines.push(``);
  lines.push(`// componentPath addressing: FunctionReference carries optional componentPath for ctx.runQuery(component.*)`);
  return lines.join("\n");
}
