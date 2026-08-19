//! Canonical server-language registry — one home per fact.
//! `ModuleLanguage` provenance tag ↔ `ModuleEnvironment` runtime tag.
//! Mirrored in `bundler/index.ts:WASM_GUEST_EXTENSIONS` and
//! `wasm_runner/validation.rs:ALLOWED_WASM_EXTENSIONS`.
//! Exhaustive `match` forces compile error on new variant.

use std::fmt;
use std::str::FromStr;

use common::types::ModuleEnvironment;

/// Server language. Wasm guests all map to `ModuleEnvironment::Wasm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleLanguage {
    TS,
    RustWasm,
    GoWasm,
    Zig,
    CWasm,
    KotlinWasm,
    DartWasm,
    CSharpWasm,
}

/// All Wasm guest extensions (mirrors `WASM_GUEST_EXTENSIONS` in bundler).
pub const ALL_WASM_EXTENSIONS: &[&str] = &[".rs", ".go", ".zig", ".c", ".kt", ".dart", ".cs"];

impl ModuleLanguage {
    pub fn from_extension(path: &str) -> Option<Self> {
        let ext = std::path::Path::new(path).extension()?.to_str()?;
        match format!(".{}", ext.to_lowercase()).as_str() {
            ".ts" | ".tsx" | ".mts" | ".cts" | ".js" | ".mjs" | ".cjs" | ".jsx" => Some(Self::TS),
            ".rs" => Some(Self::RustWasm),
            ".go" => Some(Self::GoWasm),
            ".zig" => Some(Self::Zig),
            ".c" => Some(Self::CWasm),
            ".kt" => Some(Self::KotlinWasm),
            ".dart" => Some(Self::DartWasm),
            ".cs" => Some(Self::CSharpWasm),
            _ => None,
        }
    }

    pub fn is_wasm(&self) -> bool {
        match self {
            Self::TS => false,
            Self::RustWasm | Self::GoWasm | Self::Zig | Self::CWasm | Self::KotlinWasm | Self::DartWasm | Self::CSharpWasm => true,
        }
    }

    pub fn environment(&self) -> ModuleEnvironment {
        match self {
            Self::TS => ModuleEnvironment::Isolate,
            Self::RustWasm | Self::GoWasm | Self::Zig | Self::CWasm | Self::KotlinWasm | Self::DartWasm | Self::CSharpWasm => ModuleEnvironment::Wasm,
        }
    }
}

pub fn is_wasm_environment(env: &ModuleEnvironment) -> bool {
    match env {
        ModuleEnvironment::Wasm => true,
        ModuleEnvironment::Isolate | ModuleEnvironment::Node | ModuleEnvironment::Invalid => false,
    }
}

pub fn is_wasm_guest_path(path: &str) -> bool {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{}", s.to_lowercase()))
        .unwrap_or_default();
    ALL_WASM_EXTENSIONS.contains(&ext.as_str())
}

impl fmt::Display for ModuleLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::TS => "TS",
            Self::RustWasm => "RustWasm",
            Self::GoWasm => "GoWasm",
            Self::Zig => "Zig",
            Self::CWasm => "CWasm",
            Self::KotlinWasm => "KotlinWasm",
            Self::DartWasm => "DartWasm",
            Self::CSharpWasm => "CSharpWasm",
        };
        write!(f, "{s}")
    }
}

impl FromStr for ModuleLanguage {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "TS" | "TypeScript" => Ok(Self::TS),
            "RustWasm" => Ok(Self::RustWasm),
            "GoWasm" => Ok(Self::GoWasm),
            "Zig" => Ok(Self::Zig),
            "CWasm" => Ok(Self::CWasm),
            "KotlinWasm" => Ok(Self::KotlinWasm),
            "DartWasm" => Ok(Self::DartWasm),
            "CSharpWasm" => Ok(Self::CSharpWasm),
            _ => anyhow::bail!("Unknown ModuleLanguage {s}"),
        }
    }
}
