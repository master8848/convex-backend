//! Spike: prove that the workspace's wasmtime 47.0.3 (gc feature on by
//! default) can compile and run WasmGC modules (struct/array/i31/ref), using
//! the exact Config wasm_runner uses (nan canonicalization, relaxed-simd off,
//! fuel on). Run: cargo run -p wasm_runner --example gc_spike
use wasmtime::{Config, Engine, Module, Store};

const GC_WAT: &str = r#"
(module
  ;; struct with two i32 fields
  (type $pair (struct (field $x i32) (field $y i32)))
  ;; array of mutable i32
  (type $ints (array (mut i32)))
  ;; i31ref usage: wrap/unwrap an i31
  (func (export "i31_roundtrip") (param i32) (result i32)
    local.get 0
    ref.i31
    ref.as_non_null
    i31.get_s)
  (func (export "sum") (result i32)
    (local $p (ref null $pair))
    (local $a (ref null $ints))
    ;; build a GC struct {x: 3, y: 4}
    i32.const 3
    i32.const 4
    struct.new $pair
    local.set $p
    ;; build a GC array [10, 20, 30]
    i32.const 3
    array.new_default $ints
    local.set $a
    local.get $a
    i32.const 0
    i32.const 10
    array.set $ints
    local.get $a
    i32.const 1
    i32.const 20
    array.set $ints
    local.get $a
    i32.const 2
    i32.const 30
    array.set $ints
    ;; result = x + y + arr[0] + arr[1] + arr[2] + i31(5)
    local.get $p
    struct.get $pair $x
    local.get $p
    struct.get $pair $y
    i32.add
    local.get $a
    i32.const 0
    array.get $ints
    i32.add
    local.get $a
    i32.const 1
    array.get $ints
    i32.add
    local.get $a
    i32.const 2
    array.get $ints
    i32.add
    i32.const 5
    ref.i31
    ref.as_non_null
    i31.get_s
    i32.add)
  ;; equality: two distinct structs with same payload are not ref-equal
  (func (export "ref_eq") (result i32)
    (local $a (ref null $pair))
    (local $b (ref null $pair))
    i32.const 1
    i32.const 2
    struct.new $pair
    local.set $a
    i32.const 1
    i32.const 2
    struct.new $pair
    local.set $b
    local.get $a
    local.get $b
    ref.eq
    ;; invert so we return 1 when they differ (as expected)
    i32.eqz)
)
"#;

fn main() -> anyhow::Result<()> {
    // Same knobs as crates/wasm_runner/src/engine.rs WasmRunner::new.
    let mut config = Config::new();
    config
        .cranelift_nan_canonicalization(true)
        .wasm_relaxed_simd(false)
        .consume_fuel(true);
    let engine = Engine::new(&config)?;

    // Module::new accepts .wat text directly; GC is enabled by default
    // (Config::wasm_gc is `true` by default, and the `gc`/`gc-*` Cargo
    // features are in wasmtime 47.0.3's default feature set).
    let module = Module::new(&engine, GC_WAT)?;
    println!("compiled GC module OK (imports: {:?})", module.imports().count());

    let mut store = Store::new(&engine, ());
    store.set_fuel(1_000_000)?;
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;

    let sum = instance.get_typed_func::<(), i32>(&mut store, "sum")?;
    let got = sum.call(&mut store, ())?;
    assert_eq!(got, 3 + 4 + 10 + 20 + 30 + 5, "sum mismatch");
    println!("sum() -> {got} (expected 72)");

    let rt = instance.get_typed_func::<i32, i32>(&mut store, "i31_roundtrip")?;
    for v in [-7, 0, 42, 1_000_000_000] { // i31 holds 31 bits (max ~2^30-1)
        let g = rt.call(&mut store, v)?;
        assert_eq!(g, v);
        println!("i31_roundtrip({v}) -> {g}");
    }

    let eq = instance.get_typed_func::<(), i32>(&mut store, "ref_eq")?;
    let g = eq.call(&mut store, ())?;
    assert_eq!(g, 1, "distinct GC structs should not be ref-eq");
    println!("ref_eq() -> {g} (distinct structs not equal, as expected)");

    // Now prove the knob exists and that turning GC off makes it fail:
    let mut config2 = Config::new();
    config2.wasm_gc(false);
    let engine2 = Engine::new(&config2)?;
    match Module::new(&engine2, GC_WAT) {
        Ok(_) => println!("NOTE: GC module compiled even with wasm_gc(false)"),
        Err(e) => println!("wasm_gc(false) rejects GC module as expected: {e}"),
    }
    println!("GC SPIKE OK");
    Ok(())
}
