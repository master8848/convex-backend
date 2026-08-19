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

function validatorToDartType(v: ConvexValidator, useIdType: boolean): string {
  switch (v.type) {
    case "null": return "Null";
    case "number": return "double";
    case "bigint": return "int";
    case "commitTs": return "int";
    case "boolean": return "bool";
    case "string": return "String";
    case "bytes": return "Uint8List";
    case "any": return "Object?";
    case "literal": {
      const val = v.value;
      if (val===null) return "Null";
      if (typeof val==="string") return "String";
      if (typeof val==="number") return "double";
      if (typeof val==="boolean") return "bool";
      return "String";
    }
    case "id": return useIdType ? `Id<"${v.tableName}">` : "String";
    case "array": return `List<${validatorToDartType(v.value,useIdType)}>`;
    case "record": return `Map<${validatorToDartType(v.keys,useIdType)}, ${validatorToDartType(v.values.fieldType,useIdType)}>`;
    case "union": return v.value.map(x=>validatorToDartType(x,useIdType)).join(" /* | */ ");
    case "object": return "/* object -> class */";
    default: throw new Error(`Unsupported ${(v as any).type}`);
  }
}

function dartObjectFields(
  fields: Record<string, { fieldType: ConvexValidator; optional: boolean }>,
  useIdType: boolean,
): string {
  return Object.entries(fields)
    .map(([name, f]) => {
      let t = validatorToDartType(f.fieldType, useIdType);
      if (f.optional) t = `${t}?`;
      return `  final ${t} ${name};`;
    })
    .join("\n");
}

export async function dartCodegen(
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
  lines.push(header("Generated Dart `api` utility."));
  lines.push(`// Convex Dart codegen — validator JSON is the single source of truth.`);
  lines.push(`// Value codec: \$integer/\$bytes via convexToJson/jsonToConvex equivalent (preserve int64).`);
  lines.push(`import 'dart:typed_data';`);
  lines.push(`import 'package:http/http.dart' as http;`);
  lines.push(`import 'package:web_socket_channel/web_socket_channel.dart';`);
  lines.push(`import 'package:json_annotation/json_annotation.dart';`);
  lines.push(`import 'package:freezed_annotation/freezed_annotation.dart';`);
  lines.push(``);
  lines.push(`typedef Id<T> = String;`);
  lines.push(``);
  lines.push(`class FunctionReference<Args, Ret> {`);
  lines.push(`  final String name;`);
  lines.push(`  final String visibility;`);
  lines.push(`  final String? componentPath;`);
  lines.push(`  const FunctionReference(this.name, this.visibility, [this.componentPath]);`);
  lines.push(`}`);
  lines.push(``);
  lines.push(`class ConvexClient {`);
  lines.push(`  final String deploymentUrl;`);
  lines.push(`  final http.Client httpClient;`);
  lines.push(`  ConvexClient(this.deploymentUrl, [http.Client? client]) : httpClient = client ?? http.Client();`);
  lines.push(`  Future<T> query<Args, T>(FunctionReference<Args, T> ref, Args args) => _call('query', ref, args);`);
  lines.push(`  Future<T> mutation<Args, T>(FunctionReference<Args, T> ref, Args args) => _call('mutation', ref, args);`);
  lines.push(`  Future<T> action<Args, T>(FunctionReference<Args, T> ref, Args args) => _call('action', ref, args);`);
  lines.push(`  Stream<T> subscribe<Args, T>(FunctionReference<Args, T> ref, Args args) { /* WebSocketChannel via BaseConvexClient sync protocol */ throw UnimplementedError('WebSocket subscribe stub'); }`);
  lines.push(`  Future<T> _call<Args, T>(String kind, FunctionReference<Args, T> ref, Args args) async {`);
  lines.push(`    // POST \$deploymentUrl/api/\$kind { path: ref.name, format: "convex_encoded_json", args: [convexToJson(args)] }`);
  lines.push(`    // 560 -> ConvexError`);
  lines.push(`    throw UnimplementedError('http + convex codec stub');`);
  lines.push(`  }`);
  lines.push(`  // anyApi proxy via noSuchMethod: anyApi.messages.list resolves to FunctionReference`);
  lines.push(`}`);
  lines.push(``);
  lines.push(`dynamic get anyApi => _AnyApi();`);
  lines.push(`class _AnyApi {`);
  lines.push(`  dynamic noSuchMethod(Invocation inv) => _AnyApiNode(inv.memberName.toString());`);
  lines.push(`}`);
  lines.push(`class _AnyApiNode {`);
  lines.push(`  final String path; _AnyApiNode(this.path);`);
  lines.push(`  dynamic noSuchMethod(Invocation inv) => FunctionReference(path + ':' + inv.memberName.toString().replace('Symbol("','').replace('")',''), 'public');`);
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
        lines.push(`@JsonSerializable()`);
        lines.push(`class ${name} {`);
        lines.push(dartObjectFields(v.value, useIdType));
        lines.push(`  ${name}({${Object.keys(v.value).map(k=> (v.value[k].optional? "": "required ")+`this.${k}`).join(", ")}});`);
        lines.push(`  factory ${name}.fromJson(Map<String, dynamic> json) => _\$${name}FromJson(json);`);
        lines.push(`  Map<String, dynamic> toJson() => _\$${name}ToJson(this);`);
        lines.push(`}`);
        lines.push(``);
      }
    }
  }

  // api / internal as nested objects preserving FilterApi partition
  lines.push(`class Api {`);
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    const publicFns = mod.functions.filter(f=>f.visibility?.kind==="public");
    if (publicFns.length===0) continue;
    lines.push(`  static const ${modName} = _${modName}Api();`);
  }
  lines.push(`}`);
  lines.push(``);
  for (const [modulePath, mod] of modules) {
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    const publicFns = mod.functions.filter(f=>f.visibility?.kind==="public");
    if (publicFns.length===0) continue;
    lines.push(`class _${modName}Api {`);
    lines.push(`  const _${modName}Api();`);
    for (const fn of publicFns) {
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType="Object?"; let retType="Object?";
      try {
        if (argsV) {
          if (argsV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`;
            argsType = emitted.has(cn)? cn : validatorToDartType(argsV,true);
          } else argsType=validatorToDartType(argsV,true);
        }
      } catch {}
      try {
        if (retV) {
          if (retV.type==="object") {
            const cn = `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`;
            retType = emitted.has(cn)? cn : validatorToDartType(retV,true);
          } else retType=validatorToDartType(retV,true);
        }
      } catch {}
      lines.push(`  FunctionReference<${argsType}, ${retType}> get ${fn.name} => const FunctionReference('${base}:${fn.name}', 'public');`);
    }
    lines.push(`}`);
    lines.push(``);
  }

  lines.push(`class Internal {`);
  for (const [modulePath, mod] of modules) {
    const internal = mod.functions.filter(f=>f.visibility?.kind==="internal");
    if (internal.length===0) continue;
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    lines.push(`  static const ${modName} = _${modName}Internal();`);
  }
  lines.push(`}`);
  lines.push(``);
  for (const [modulePath, mod] of modules) {
    const internal = mod.functions.filter(f=>f.visibility?.kind==="internal");
    if (internal.length===0) continue;
    const base = importPath(modulePath);
    const modName = base.split("/").pop()!.replace(/[^a-zA-Z0-9_]/g,"_");
    lines.push(`class _${modName}Internal {`);
    lines.push(`  const _${modName}Internal();`);
    for (const fn of internal) {
      const argsV = parseValidator(fn.args);
      const retV = parseValidator(fn.returns);
      let argsType="Object?"; let retType="Object?";
      try { if(argsV) argsType=argsV.type==="object"? `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Args`:validatorToDartType(argsV,true); } catch{}
      try { if(retV) retType=retV.type==="object"? `${base.split("/").map(s=>s.charAt(0).toUpperCase()+s.slice(1)).join("")}_${fn.name}_Return`:validatorToDartType(retV,true); } catch{}
      lines.push(`  FunctionReference<${argsType}, ${retType}> get ${fn.name} => const FunctionReference('${base}:${fn.name}', 'internal');`);
    }
    lines.push(`}`);
    lines.push(``);
  }
  lines.push(`// componentPath addressing carried via FunctionReference.componentPath`);
  return lines.join("\n");
}
