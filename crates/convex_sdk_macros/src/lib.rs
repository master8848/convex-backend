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
//! - `__convex_functions() -> i32`: the list of `{"name", "type"}`
//!   descriptors.

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
        return syn::Error::new(
            Span::call_site(),
            "convex_functions takes no arguments",
        )
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
                "convex_functions requires at least one function annotated with \
                 #[query], #[mutation], #[action], or #[http_action]",
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
        quote! {
            convex_sdk::rt::FunctionDescriptor {
                name: #name_str,
                function_type: #function_type,
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
            arg_deserialization.push(quote! {
                let #pat: #ty = serde_json::from_value(args[#arg_index].clone())
                    .map_err(|e| format!("{}: {}", #invalid_message, e))?;
            });
            call_args.push(quote! { #pat });
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
}
