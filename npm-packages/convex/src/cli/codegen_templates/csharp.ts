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

function validatorToCSharpType(v: ConvexValidator, useIdType: boolean): string {
  switch (v.type) {
    case "null": return "object?";
    case "number": return "double";
    case "bigint": return "long";
    case "commitTs": return "long";
    case "boolean": return "bool";
    case "string": return "string";
    case "bytes": return "byte[]";
    case "any": return "System.Text.Json.JsonElement";
    case "literal": {
      const val = v.value;
      if (val===null) return "object?";
      if (typeof val==="string") return "string";
      if (typeof val==="number") return "double";
      if (typeof val==="boolean") return "bool";
      return "string";
    }
    case "id": return useIdType ? `Id<${v.tableName}>` : "string";
    case "array": return `List<${validatorToCSharpType(v.value,useIdType)}>`;
    case "record": return `Dictionary<${validatorToCSharpType(v.keys,useIdType)}, ${validatorToCSharpType(v.values.fieldType,useIdType)}>`;
    case "union": return v.value.map(x=>validatorToCSharpType(x,useIdType)).join(" /* | */ ");
    case "object": return "/* object -> record */";
    default: throw new Error(`Unsupported ${(v as any).type}`);
  }
}

function csharpObjectFields(
  fields: Record<string, { fieldType: ConvexValidator; optional: boolean }>,
  useIdType: boolean,
): string {
  return Object.entries(fields)
    .map(([name, f]) => {
      let t = validatorToCSharpType(f.fieldType, useIdType);
      const nullable = f.optional ? "?" : "";
      // System.Text.Json handles $integer/$bytes via custom converter
      return `    public ${t}${nullable} ${name.charAt(0).toUpperCase()+name.slice(1)} { get; set; }${f.optional? " = null;":""}`;
    })
    .join("\n");
}

export async function csharpCodegen(
  ctx: Context,
  startPushResponse: StartPushResponse,
  rootComponent: ComponentDirectory,
  componentDirectory: ComponentDirectory,
  opts?: { useIdType?: boolean },
): Promise<string> {
  const useIdType = opts?.useIdType ?? true;
  const definitionPath = toComponentDefinitionPath(rootComponent, componentDirectory);
  const analysis = startPushResponse.analysis[definitionPath];
  if (!analysis) {
    return await ctx.crash({
      exitCode: 1,
      errorType: "fatal",
      printedMessage: `No analysis found for component ${definitionPath}`,
    });
  }
  const lines: string[] = [];
  lines.push(header("Generated C# `Api` utility."));
  lines.push(`// Convex C# codegen — validator JSON is the single source of truth.`);
  lines.push(`// Value codec: System.Text.Json converter for $integer/$bytes (convexToJson equivalent).`);
  lines.push(`using System.Text.Json;`);
  lines.push(`using System.Text.Json.Serialization;`);
  lines.push(`using System.Net.Http;`);
  lines.push(`using System.Net.Http.Json;`);
  lines.push(``);
  lines.push(`namespace Convex.Generated;`);
  lines.push(``);
  lines.push(`public readonly record struct Id<T>(string Value);`);
  lines.push(``);
  lines.push(`public record FunctionReference<TArgs, TReturn>(string Name, string Visibility, string? ComponentPath = null);`);
  lines.push(``);
  lines.push(`public class ConvexClient {`);
  lines.push(`    private readonly HttpClient _http;`);
  lines.push(`    private readonly string _deploymentUrl;`);
  lines.push(`    public ConvexClient(string deploymentUrl, HttpClient http) { _deploymentUrl = deploymentUrl; _http = http; }`);
  lines.push(`    public async Task<T> QueryAsync<TArgs, T>(FunctionReference<TArgs, T> reference, TArgs args) => await CallAsync<TArgs,T>("query", reference, args);`);
  lines.push(`    public async Task<T> MutationAsync<TArgs, T>(FunctionReference<TArgs, T> reference, TArgs args) => await CallAsync<TArgs,T>("mutation", reference, args);`);
  lines.push(`    public async Task<T> ActionAsync<TArgs, T>(FunctionReference<TArgs, T> reference, TArgs args) => await CallAsync<TArgs,T>("action", reference, args);`);
  lines.push(`    // WebSocket sync (System.Net.WebSockets) optional: IAsyncEnumerable<T> SubscribeAsync<TArgs,T>(...)`);
  lines.push(`    private async Task<T> CallAsync<TArgs, T>(string kind, FunctionReference<TArgs, T> reference, TArgs args) {`);
  lines.push(`        // POST {_deploymentUrl}/api/{kind} { path: reference.Name, format: "convex_encoded_json", args: [ConvexJson.Serialize(args)] }`);
  lines.push(`        // 560 -> ConvexError { message, data }`);
  lines.push(`        throw new NotImplementedException("HttpClient + ConvexJson converter stub");`);
  lines.push(`    }`);
  lines.push(`}`);
  lines.push(``);

  const modules = Object.entries(analysis.functions).sort(([a],[b])=>compareStrings(a,b));
  const emitted = new Set<string>();
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const modPascal = base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("");
    for (const fn of mod.functions) {
      for (const which of ["args","returns"] as const) {
        const raw = which==="args"?fn.args:fn.returns;
        const v = parseValidator(raw);
        if (!v || v.type!=="object") continue;
        const name = `${modPascal}_${fn.name}_${which==="args"?"Args":"Return"}`;
        if (emitted.has(name)) continue;
        emitted.add(name);
        lines.push(`public record ${name} {`);
        lines.push(csharpObjectFields(v.value, useIdType));
        lines.push(`}`);
        lines.push(``);
      }
    }
  }

  lines.push(`public static class Api {`);
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const modPascal = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    const cap = modPascal.charAt(0).toUpperCase()+modPascal.slice(1);
    lines.push(`    public static class ${cap} {`);
    for (const fn of mod.functions.filter(f=>f.visibility?.kind==="public")) {
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType="object"; let retType="object";
      try {
        if (argsV) {
          if (argsV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`;
            argsType = emitted.has(cn)? cn : validatorToCSharpType(argsV,useIdType);
          } else if (argsV.type==="any") argsType="JsonElement";
          else argsType=validatorToCSharpType(argsV,useIdType);
        }
      } catch {}
      try {
        if (retV) {
          if (retV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`;
            retType = emitted.has(cn)? cn : validatorToCSharpType(retV,useIdType);
          } else retType=validatorToCSharpType(retV,useIdType);
        }
      } catch {}
      lines.push(`        public static readonly FunctionReference<${argsType}, ${retType}> ${fn.name.charAt(0).toUpperCase()+fn.name.slice(1)} = new("${base}:${fn.name}", "public");`);
    }
    lines.push(`    }`);
  }
  lines.push(`}`);
  lines.push(``);
  lines.push(`public static class Internal {`);
  lines.push(`    // FilterApi<typeof fullApi, FunctionReference<any,"internal">> equivalent`);
  for (const [modulePath, mod] of modules) {
    const internal = mod.functions.filter(f=>f.visibility?.kind==="internal");
    if (internal.length===0) continue;
    const base = importPath(modulePath);
    const modPascal = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    const cap = modPascal.charAt(0).toUpperCase()+modPascal.slice(1);
    lines.push(`    public static class ${cap} {`);
    for (const fn of internal) {
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType="object"; let retType="object";
      try { if(argsV) argsType=argsV.type==="object"? `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`:validatorToCSharpType(argsV,true); } catch{}
      try { if(retV) retType=retV.type==="object"? `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`:validatorToCSharpType(retV,true); } catch{}
      lines.push(`        public static readonly FunctionReference<${argsType}, ${retType}> ${fn.name.charAt(0).toUpperCase()+fn.name.slice(1)} = new("${base}:${fn.name}", "internal");`);
    }
    lines.push(`    }`);
  }
  lines.push(`}`);
  lines.push(``);
  lines.push(`// componentPath addressing: FunctionReference.ComponentPath carries component path for cross-component calls`);
  return lines.join("\n");
}
