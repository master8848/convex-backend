// Client codegen language registry — single source of truth for --lang.
// One-home for CodegenLang, choices, and per-language dispatch.
// Adding a language (<1 day):
//  1) create `codegen_templates/<lang>.ts` with validatorToXType + <lang>Codegen
//  2) add entry to CODEGEN_DISPATCH with { outputFile, gen }
//  3) no other switches needed — CLI and doFinalComponentCodegen read this map.
// Contract: validator JSON sole truth via parseValidator, preserve FilterApi
// public/internal + componentPath via FunctionReference, transport POST
// /api/{query,mutation,action} format convex_encoded_json, $integer/$bytes, 560 → ConvexError.
import { kotlinCodegen } from "../codegen_templates/kotlin.js";
import { rustCodegen } from "../codegen_templates/rust.js";
import { csharpCodegen } from "../codegen_templates/csharp.js";
import { dartCodegen } from "../codegen_templates/dart.js";
import type { Context } from "../../bundler/context.js";
import type { StartPushResponse } from "./deployApi/startPush.js";
import type { ComponentDirectory } from "./components/definition/directoryStructure.js";

export const CODEGEN_LANGS = [
  "typescript",
  "kotlin",
  "rust",
  "csharp",
  "dart",
] as const;
export type CodegenLang = (typeof CODEGEN_LANGS)[number];

// CLI choices include alias "cs" -> "csharp" for backwards compat.
export const CODEGEN_LANG_CHOICES = [
  ...CODEGEN_LANGS,
  "cs",
] as const;

export function isCodegenLang(v: string): v is CodegenLang {
  return (CODEGEN_LANGS as readonly string[]).includes(v);
}
export function normalizeLang(raw: string): CodegenLang {
  if (raw === "cs") return "csharp";
  return raw as CodegenLang;
}

export type CodegenFn = (
  ctx: Context,
  startPushResponse: StartPushResponse,
  rootComponent: ComponentDirectory,
  componentDirectory: ComponentDirectory,
) => Promise<string>;

export const CODEGEN_DISPATCH: Record<
  Exclude<CodegenLang, "typescript">,
  { outputFile: string; gen: CodegenFn }
> = {
  kotlin: { outputFile: "Api.kt", gen: kotlinCodegen },
  rust: { outputFile: "api.rs", gen: rustCodegen },
  csharp: { outputFile: "Api.cs", gen: csharpCodegen },
  dart: { outputFile: "api.dart", gen: dartCodegen },
};

export function getCodegenConfig(lang: CodegenLang) {
  if (lang === "typescript") return null;
  return CODEGEN_DISPATCH[lang as Exclude<CodegenLang, "typescript">] ?? null;
}
export function outputFileForLang(lang: CodegenLang): string | null {
  const c = getCodegenConfig(lang);
  return c ? c.outputFile : null;
}
