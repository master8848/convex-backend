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

function validatorToRustType(v: ConvexValidator, useIdType: boolean): string {
  switch (v.type) {
    case "null": return "()";
    case "number": return "f64";
    case "bigint": return "i64";
    case "commitTs": return "i64";
    case "boolean": return "bool";
    case "string": return "String";
    case "bytes": return "Vec<u8>";
    case "any": return "serde_json::Value";
    case "literal": {
      const val = v.value;
      if (val === null) return "()";
      if (typeof val === "string") return "String";
      if (typeof val === "number") return "f64";
      if (typeof val === "boolean") return "bool";
      return "String";
    }
    case "id": return useIdType ? `Id<"${v.tableName}">` : "String";
    case "array": return `Vec<${validatorToRustType(v.value, useIdType)}>`;
    case "record": return `std::collections::HashMap<${validatorToRustType(v.keys, useIdType)}, ${validatorToRustType(v.values.fieldType, useIdType)}>`;
    case "union": return v.value.map(x => validatorToRustType(x, useIdType)).join(" /* | */ ");
    case "object": return "/* object -> struct */";
    default: throw new Error(`Unsupported ${(v as any).type}`);
  }
}

function rustObjectFields(
  fields: Record<string, { fieldType: ConvexValidator; optional: boolean }>,
  useIdType: boolean,
): string {
  return Object.entries(fields)
    .map(([name, f]) => {
      let t = validatorToRustType(f.fieldType, useIdType);
      if (f.optional) t = `Option<${t}>`;
      // serde rename for camel? Keep as is.
      return `    pub ${name}: ${t},`;
    })
    .join("\n");
}

export async function rustCodegen(
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
  lines.push(header("Generated Rust `api` utility."));
  lines.push(`//! Convex Rust codegen — validator JSON is the single source of truth.`);
  lines.push(`//! Value codec preserves \$integer/\$bytes via convexToJson/jsonToConvex equivalent (serde).`);
  lines.push(`use serde::{Serialize, Deserialize};`);
  lines.push(`use std::collections::HashMap;`);
  lines.push(``);
  lines.push(`#[derive(Debug, Clone, Serialize, Deserialize)]`);
  lines.push(`pub struct Id<T = ()>(pub String);`);
  lines.push(``);
  lines.push(`/// FunctionReference mirrors src/server/api.ts:431 anyApi proxy and FunctionReference type`);
  lines.push(`/// plus componentPath for ctx.runQuery(component.*) addressing.`);
  lines.push(`#[derive(Debug, Clone)]`);
  lines.push(`pub struct FunctionReference<Args, Ret> {`);
  lines.push(`    pub name: &'static str,`);
  lines.push(`    pub visibility: &'static str,`);
  lines.push(`    pub component_path: Option<&'static str>,`);
  lines.push(`    pub _marker: std::marker::PhantomData<(Args, Ret)>,`);
  lines.push(`}`);
  lines.push(``);
  lines.push(`pub struct ConvexClient {`);
  lines.push(`    pub deployment_url: String,`);
  lines.push(`    pub http: reqwest::Client,`);
  lines.push(`}`);
  lines.push(`impl ConvexClient {`);
  lines.push(`    pub fn new(deployment_url: impl Into<String>) -> Self { Self { deployment_url: deployment_url.into(), http: reqwest::Client::new() } }`);
  lines.push(`    pub async fn query<Args: Serialize, Ret: for<'de> Deserialize<'de>>(&self, reference: &FunctionReference<Args, Ret>, args: Args) -> anyhow::Result<Ret> { self.call("query", reference, args).await }`);
  lines.push(`    pub async fn mutation<Args: Serialize, Ret: for<'de> Deserialize<'de>>(&self, reference: &FunctionReference<Args, Ret>, args: Args) -> anyhow::Result<Ret> { self.call("mutation", reference, args).await }`);
  lines.push(`    pub async fn action<Args: Serialize, Ret: for<'de> Deserialize<'de>>(&self, reference: &FunctionReference<Args, Ret>, args: Args) -> anyhow::Result<Ret> { self.call("action", reference, args).await }`);
  lines.push(`    async fn call<Args: Serialize, Ret: for<'de> Deserialize<'de>>(&self, kind: &str, reference: &FunctionReference<Args, Ret>, args: Args) -> anyhow::Result<Ret> {`);
  lines.push(`        // POST {deployment_url}/api/{kind} { path: reference.name, format: "convex_encoded_json", args: [convex_to_json(args)] }`);
  lines.push(`        // 560 -> ConvexError { message, data }`);
  lines.push(`        let _ = (kind, reference, args);`);
  lines.push(`        anyhow::bail!("ConvexClient HTTP stub — fill with reqwest + convex codec")`);
  lines.push(`    }`);
  lines.push(`}`);
  lines.push(``);

  const modules = Object.entries(analysis.functions).sort(([a],[b])=>compareStrings(a,b));
  // Emit structs for object validators
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
        lines.push(`#[derive(Debug, Clone, Serialize, Deserialize)]`);
        lines.push(`pub struct ${name} {`);
        lines.push(rustObjectFields(v.value, useIdType));
        lines.push(`}`);
        lines.push(``);
      }
    }
  }

  lines.push(`pub mod api {`);
  lines.push(`    use super::*;`);
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    lines.push(`    pub mod ${modName} {`);
    lines.push(`        use super::super::*;`);
    for (const fn of mod.functions) {
      const vis = fn.visibility?.kind ?? "public";
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType = "()"; let retType = "()";
      try {
        if (argsV) {
          if (argsV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`;
            argsType = emitted.has(cn)? `super::super::${cn}` : validatorToRustType(argsV,useIdType);
          } else if (argsV.type==="any") argsType="serde_json::Value";
          else argsType=validatorToRustType(argsV,useIdType);
        }
      } catch {}
      try {
        if (retV) {
          if (retV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`;
            retType = emitted.has(cn)? `super::super::${cn}` : validatorToRustType(retV,useIdType);
          } else retType=validatorToRustType(retV,useIdType);
        }
      } catch {}
      const udfType = fn.udfType.toLowerCase();
      lines.push(`        // ${vis} ${udfType} ${base}:${fn.name}`);
      lines.push(`        pub static ${fn.name}: FunctionReference<${argsType}, ${retType}> = FunctionReference { name: "${base}:${fn.name}", visibility: "${vis}", component_path: None, _marker: std::marker::PhantomData };`);
    }
    lines.push(`    }`);
  }
  lines.push(`}`);
  lines.push(``);
  lines.push(`pub mod internal {`);
  lines.push(`    // FilterApi<typeof fullApi, FunctionReference<any,"internal">> equivalent — only internal vis`);
  for (const [modulePath, mod] of modules) {
    const internalFns = mod.functions.filter(f=>f.visibility?.kind==="internal");
    if (internalFns.length===0) continue;
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    lines.push(`    pub mod ${modName} {`);
    lines.push(`        use super::super::*;`);
    for (const fn of internalFns) {
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType="()"; let retType="()";
      try { if(argsV) argsType=argsV.type==="object"? `super::super::${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`:validatorToRustType(argsV,true); } catch{}
      try { if(retV) retType=retV.type==="object"? `super::super::${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`:validatorToRustType(retV,true); } catch{}
      lines.push(`        pub static ${fn.name}: FunctionReference<${argsType}, ${retType}> = FunctionReference { name: "${base}:${fn.name}", visibility: "internal", component_path: None, _marker: std::marker::PhantomData };`);
    }
    lines.push(`    }`);
  }
  lines.push(`}`);
  return lines.join("\n");
}
