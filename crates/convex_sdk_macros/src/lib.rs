//! Procedural macros for the [`convex_sdk`] Rust SDK.
//!
//! The `convex_functions` attribute macro transforms a module of annotated
//! functions into a WASM module exportable to the Convex backend:
//!
//! ```ignore
//! use convex_sdk::{convex_functions, query, Context, ConvexError};
//!
//! #[convex_functions]
//! pub mod functions {
//!     #[query]
//!     pub async fn list(ctx: Context, prefix: String) -> Result<Vec<String>, ConvexError> {
//!         // ...
//!         Ok(vec![])
//!     }
//! }
//! ```
//!
//! For each function annotated with `#[query]`, `#[mutation]`, `#[action]`,
//! or `#[http_action]`, the macro generates:
//!
//! - A wrapper that deserializes the function's arguments from the input
//!   payload, calls the user function, and serializes the result.
//! - An entry in the module's function registry.
//!
//! It also generates the two WASM exports the backend expects:
//!
//! - `__convex_run() -> i32`: the dispatcher.
//! - `__convex_functions() -> i32`: the list of `{"name", "type"}` descriptors.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{
    format_ident,
    quote,
};
use syn::{
    parse_macro_input,
    Attribute,
    FnArg,
    ItemMod,
    Pat,
    ReturnType,
    Type,
};

/// The functions this macro recognizes on inner functions.
const FUNCTION_TYPES: &[(&str, &str)] = &[
    ("query", "query"),
    ("mutation", "mutation"),
    ("action", "action"),
    ("http_action", "httpAction"),
];

/// True if the type is `Context` (or `convex_sdk::Context`).
fn is_context_type(ty: &Type) -> bool {
    let Type::Path(type_path) = ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Context")
}

/// Extract the function type annotation ("query", "mutation", ...) from the
/// attribute list, if any.
fn function_type_from_attrs(attrs: &[Attribute]) -> Option<(&str, &str)> {
    attrs.iter().find_map(|attr| {
        let segment = attr.path().segments.last()?;
        let name = segment.ident.to_string();
        FUNCTION_TYPES
            .iter()
            .find(|(annotation, _)| *annotation == name)
            .copied()
    })
}

/// A no-op attribute macro accepted for readability.
///
/// The `convex_functions` macro consumes these annotations; this macro exists
/// only so `#[query]` resolves when used directly.
#[proc_macro_attribute]
pub fn query(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// A no-op attribute macro accepted for readability. See [`query`].
#[proc_macro_attribute]
pub fn mutation(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// A no-op attribute macro accepted for readability. See [`query`].
#[proc_macro_attribute]
pub fn action(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// A no-op attribute macro accepted for readability. See [`query`].
#[proc_macro_attribute]
pub fn http_action(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// The `convex_functions` attribute macro for `mod` items.
#[proc_macro_attribute]
pub fn convex_functions(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut module = parse_macro_input!(item as ItemMod);
    if !attr.is_empty() {
        return syn::Error::new(Span::call_site(), "convex_functions takes no arguments")
            .to_compile_error()
            .into();
    }

    let Some((_, items)) = &mut module.content else {
        return syn::Error::new(
            module.ident.span(),
            "convex_functions can only be applied to inline modules",
        )
        .to_compile_error()
        .into();
    };

    // Collect the annotated functions and their signatures.
    let mut functions = Vec::new();
    for item in items.iter_mut() {
        let syn::Item::Fn(func) = item else {
            continue;
        };
        let Some((annotation, function_type)) = function_type_from_attrs(&func.attrs) else {
            continue;
        };
        let (annotation, function_type) = (annotation.to_string(), function_type.to_string());
        // Strip the annotation so it doesn't need to resolve to a real macro.
        func.attrs.retain(|attr| {
            !attr
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == annotation)
        });

        let name = func.sig.ident.clone();
        let is_async = func.sig.asyncness.is_some();
        let return_ty = match &func.sig.output {
            ReturnType::Type(_, ty) => (**ty).clone(),
            ReturnType::Default => syn::parse_quote! { () },
        };
        let is_result_return = matches!(
            &return_ty,
            Type::Path(type_path)
                if type_path.path.segments.last().is_some_and(|s| s.ident == "Result")
        );

        // Split parameters into (context, user args).
        let mut has_context = false;
        let mut args: Vec<(Pat, Type)> = Vec::new();
        for (index, input) in func.sig.inputs.iter().enumerate() {
            let FnArg::Typed(pat_type) = input else {
                continue;
            };
            if index == 0 && is_context_type(&pat_type.ty) {
                has_context = true;
                continue;
            }
            args.push(((*pat_type.pat).clone(), (*pat_type.ty).clone()));
        }

        functions.push(FunctionSpec {
            name,
            function_type: function_type.to_string(),
            is_async,
            has_context,
            args,
            return_ty,
            is_result_return,
        });
    }

    let mut errors = Vec::new();
    if functions.is_empty() {
        errors.push(
            syn::Error::new(
                module.ident.span(),
                "convex_functions requires at least one function annotated with #[query], \
                 #[mutation], #[action], or #[http_action]",
            )
            .to_compile_error(),
        );
    }

    let wrappers = functions.iter().map(|spec| spec.wrapper());
    let registry_entries = functions.iter().map(|spec| {
        let name_str = spec.name.to_string();
        let wrapped_name = spec.wrapped_name();
        quote! { (#name_str, #wrapped_name) }
    });
    let descriptors = functions.iter().map(|spec| {
        let name_str = spec.name.to_string();
        let function_type = &spec.function_type;
        let args_json = spec.args_validator_json();
        let returns_json = spec.returns_validator_json();
        // args and returns are JSON strings (ConvexValidator IR) — same shape as
        // WasmFunctionDescriptor {args: Option<String>} so analyzer can build
        // Identical AnalyzedFunction rows as the isolate path.
        quote! {
            convex_sdk::rt::FunctionDescriptor {
                name: #name_str,
                function_type: #function_type,
                args: Some(#args_json),
                returns: Some(#returns_json),
                visibility: Some("public"),
            }
        }
    });

    let generated = quote! {
        #(#wrappers)*

        static __CONVEX_FUNCTIONS: &[(&str, convex_sdk::rt::WrappedFn)] = &[
            #(#registry_entries,)*
        ];

        static __CONVEX_FUNCTION_DESCRIPTORS: &[convex_sdk::rt::FunctionDescriptor] = &[
            #(#descriptors,)*
        ];

        #[unsafe(no_mangle)]
        pub extern "C" fn __convex_run() -> i32 {
            convex_sdk::rt::dispatch(&__CONVEX_FUNCTIONS)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn __convex_functions() -> i32 {
            convex_sdk::rt::functions_output(&__CONVEX_FUNCTION_DESCRIPTORS)
        }
    };

    // Everything is emitted inside the module so wrappers can call the user
    // functions by name.
    let generated_items = syn::parse2::<syn::File>(generated)
        .expect("generated convex_functions code should be valid")
        .items;
    items.extend(generated_items);
    let expanded = quote! {
        #module
        #(#errors)*
    };
    TokenStream::from(expanded)
}

/// A user function annotated with one of the convex function types.
struct FunctionSpec {
    name: syn::Ident,
    function_type: String,
    is_async: bool,
    has_context: bool,
    args: Vec<(Pat, Type)>,
    return_ty: Type,
    is_result_return: bool,
}

impl FunctionSpec {
    fn wrapped_name(&self) -> syn::Ident {
        format_ident!("__convex_wrap_{}", self.name)
    }

    /// Generate the wrapper function:
    ///
    /// ```ignore
    /// fn __convex_wrap_list(args: &[serde_json::Value])
    ///     -> Result<serde_json::Value, String> {
    ///     convex_sdk::rt::block_on(async move {
    ///         let ctx = convex_sdk::Context::new();
    ///         let prefix: String = serde_json::from_value(args[0].clone())?;
    ///         let result = list(ctx, prefix).await;
    ///         match result { Ok(v) => ..., Err(e) => Err(e.to_string()) }
    ///     })
    /// }
    /// ```
    fn wrapper(&self) -> proc_macro2::TokenStream {
        let name = &self.name;
        let wrapped_name = self.wrapped_name();
        let is_async = self.is_async;
        let return_ty = &self.return_ty;
        let is_result_return = self.is_result_return;

        let mut arg_deserialization = Vec::new();
        let mut call_args = Vec::new();
        if self.has_context {
            call_args.push(quote! { ctx });
        }
        for (index, (pat, ty)) in self.args.iter().enumerate() {
            let arg_index = syn::Index::from(index);
            let arg_name = match pat {
                Pat::Ident(ident) => ident.ident.to_string(),
                _ => format!("argument {index}"),
            };
            let invalid_message = format!("Invalid value for {arg_name}");
            // Use hygienic ident to avoid shadowing function name (e.g. `query(query: String)`)
            let arg_ident = format_ident!("__arg{}", index);
            arg_deserialization.push(quote! {
                let #arg_ident: #ty = serde_json::from_value(args[#arg_index].clone())
                    .map_err(|e| format!("{}: {}", #invalid_message, e))?;
            });
            call_args.push(quote! { #arg_ident });
            // Keep pat for unused warning suppression: if pat is not simple ident, this is no-op
            let _ = pat;
        }
        let call_expr = if is_async {
            quote! { #name(#(#call_args),*).await }
        } else {
            quote! { #name(#(#call_args),*) }
        };

        let result_mapping = if is_result_return {
            quote! {
                match result {
                    Ok(value) => serde_json::to_value(value).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            }
        } else {
            quote! {
                serde_json::to_value(result).map_err(|e| e.to_string())
            }
        };

        let body = if is_async {
            quote! {
                convex_sdk::rt::block_on(async move {
                    let ctx = convex_sdk::Context::new();
                    #(#arg_deserialization)*
                    let result: #return_ty = #call_expr;
                    #result_mapping
                })
            }
        } else {
            quote! {
                let ctx = convex_sdk::Context::new();
                #(#arg_deserialization)*
                let result: #return_ty = #call_expr;
                #result_mapping
            }
        };

        quote! {
            #[allow(non_snake_case, unused_variables)]
            fn #wrapped_name(args: &[serde_json::Value])
                -> Result<serde_json::Value, String> {
                #body
            }
        }
    }

    /// ConvexValidator JSON for args: {"type":"object","value":{field: {fieldType, optional}}}
    /// Mirrors npm-packages/convex/src/values/validators.ts VObject json.
    fn args_validator_json(&self) -> String {
        let mut fields = serde_json::Map::new();
        for (pat, ty) in &self.args {
            let field_name = match pat {
                Pat::Ident(ident) => ident.ident.to_string(),
                _ => continue,
            };
            let (validator, optional) = validator_for_type(ty);
            let mut field_obj = serde_json::Map::new();
            field_obj.insert("fieldType".to_string(), validator);
            field_obj.insert("optional".to_string(), serde_json::Value::Bool(optional));
            fields.insert(field_name, serde_json::Value::Object(field_obj));
        }
        let mut obj = serde_json::Map::new();
        obj.insert("type".to_string(), serde_json::Value::String("object".to_string()));
        obj.insert("value".to_string(), serde_json::Value::Object(fields));
        serde_json::Value::Object(obj).to_string()
    }

    /// ConvexValidator JSON for returns. Unwraps Result<T,E> to T, then maps type to validator.
    fn returns_validator_json(&self) -> String {
        let inner = if self.is_result_return {
            extract_result_inner(&self.return_ty).unwrap_or(&self.return_ty)
        } else {
            &self.return_ty
        };
        // () or unit -> null validator (void maps to null on client)
        if is_unit_type(inner) {
            return serde_json::json!({"type":"null"}).to_string();
        }
        let (validator, _optional) = validator_for_type(inner);
        // For returns, Option<T> is encoded as union with null, not optional field
        // validator_for_type already returns optional flag; for returns we expand to union if optional
        if _optional {
            serde_json::json!({"type":"union","value":[validator, {"type":"null"}]}).to_string()
        } else {
            validator.to_string()
        }
    }
}

fn is_unit_type(ty: &Type) -> bool {
    if let Type::Tuple(tuple) = ty {
        return tuple.elems.is_empty();
    }
    if let Type::Path(p) = ty {
        if p.path.segments.len() == 1 && p.path.segments[0].ident == "()" {
            return true;
        }
    }
    false
}

fn extract_result_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else { return None; };
    let segment = path.path.segments.last()?;
    if segment.ident != "Result" { return None; }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else { return None; };
    let first = args.args.first()?;
    if let syn::GenericArgument::Type(inner) = first { Some(inner) } else { None }
}

/// Map Rust type to (ConvexValidator JSON, isOptional). Optional is true for Option<T>.
fn validator_for_type(ty: &Type) -> (serde_json::Value, bool) {
    // Strip references and handle Option<Vec<String>> recursively
    let ty_str = quote::quote!(#ty).to_string().replace(' ', "");
    // Handle Option<T>
    if let Type::Path(path) = ty {
        if let Some(seg) = path.path.segments.last() {
            if seg.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        let (inner_val, _) = validator_for_type(inner);
                        return (inner_val, true);
                    }
                }
                return (serde_json::json!({"type":"any"}), true);
            }
            // Vec<T> / VecDeque etc
            if seg.ident == "Vec" || seg.ident == "VecDeque" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        let (inner_val, _) = validator_for_type(inner);
                        // inner optional shouldn't propagate to array; use inner validator as value
                        return (serde_json::json!({"type":"array","value": inner_val}), false);
                    }
                }
                return (serde_json::json!({"type":"array","value":{"type":"any"}}), false);
            }
            // BTreeSet, HashSet similar
            if seg.ident == "BTreeSet" || seg.ident == "HashSet" || seg.ident == "HashMap" || seg.ident == "BTreeMap" {
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    // For maps, value type is last arg
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.last() {
                        let (inner_val, _) = validator_for_type(inner);
                        return (serde_json::json!({"type":"array","value": inner_val}), false);
                    }
                }
                return (serde_json::json!({"type":"any"}), false);
            }
            let ident = seg.ident.to_string();
            let val = match ident.as_str() {
                "String" | "str" | "ConvexString" => serde_json::json!({"type":"string"}),
                "bool" | "Boolean" => serde_json::json!({"type":"boolean"}),
                "f64" | "f32" | "Float64" => serde_json::json!({"type":"number"}),
                "i64" | "u64" | "i32" | "u32" | "i16" | "u16" | "i8" | "u8" | "isize" | "usize" | "Int64" => serde_json::json!({"type":"bigint"}),
                "Bytes" | "Vec_u8" => serde_json::json!({"type":"bytes"}),
                "Document" | "ConvexValue" | "JsonValue" | "Value" | "JsonElement" => serde_json::json!({"type":"any"}),
                "GenericId" | "Id" => serde_json::json!({"type":"string"}),
                "User" => serde_json::json!({"type":"any"}),
                _ => {
                    // Fallback: check string contains for generic wrappers not caught above
                    if ty_str.contains("String") { serde_json::json!({"type":"string"}) }
                    else if ty_str.contains("Vec") { serde_json::json!({"type":"array","value":{"type":"any"}}) }
                    else { serde_json::json!({"type":"any"}) }
                }
            };
            return (val, false);
        }
    }
    // Reference &T
    if let Type::Reference(r) = ty {
        return validator_for_type(&r.elem);
    }
    // Tuple etc fallback to any
    (serde_json::json!({"type":"any"}), false)
}
