# Rust guest example

The most ergonomic guest: uses the `convex_sdk` crate (crates/convex_sdk) with
`#[convex_functions]`, `#[query]`, `#[mutation]` macros. See src/lib.rs for
queries, mutations, database reads/writes, logs, deterministic randomness and
typed errors.

Build (from repo root):

```sh
rustup target add wasm32-wasip1
cargo build --manifest-path examples/wasm-guests/rust/Cargo.toml --target wasm32-wasip1 --release
# -> examples/wasm-guests/rust/target/wasm32-wasip1/release/wasm_guest_example.wasm
```

or simply `make rust` in examples/wasm-guests.
