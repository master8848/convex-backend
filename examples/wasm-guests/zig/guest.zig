//! Minimal Convex WASM guest in Zig (freestanding ABI, no JSON parsing).
//!
//! ABI contract (crates/wasm_runner/src/abi.rs):
//!   exports: __convex_run() -> i32, __convex_functions() -> i32
//!   imports (module `env`): __convex_input_length, __convex_input_load,
//!     __convex_output_set, __convex_error_set
//!
//! This is a WASI reactor module (no main / _start): the host calls
//! __convex_run / __convex_functions directly, exactly like the C guest.

const std = @import("std");

// --- Host imports (module `env`) -------------------------------------------
extern "env" fn __convex_input_length() i32;
extern "env" fn __convex_input_load(offset: i32, dest: i32, len: i32) void;
extern "env" fn __convex_output_set(ptr: i32, len: i32) void;
extern "env" fn __convex_error_set(ptr: i32, len: i32) void;

// Static guest buffers live in linear memory; their addresses are valid
// i32 offsets for the host to read/write (host-allocated-memory pattern:
// the HOST owns the input buffer, the GUEST owns these buffers).
var input_buf: [64 * 1024]u8 = undefined;

fn ptrOf(slice: []const u8) i32 {
    return @intCast(@intFromPtr(slice.ptr));
}

/// Dispatcher: pulls the JSON payload {"function": ..., "args": [...]}
/// via the input host functions and returns the first argument (a JSON
/// string literal) as the function result, like the C guest fixture.
export fn __convex_run() i32 {
    const len = __convex_input_length();
    if (len < 0 or len > input_buf.len) {
        const msg = "input too large";
        __convex_error_set(ptrOf(msg), @intCast(msg.len));
        return 1;
    }
    if (len > 0) {
        __convex_input_load(0, ptrOf(input_buf[0..]), len);
    }
    const input = input_buf[0..@intCast(len)];
    const args_marker = "\"args\": [";
    var start: usize = 0;
    while (start + args_marker.len <= input.len) : (start += 1) {
        if (std.mem.eql(u8, input[start .. start + args_marker.len], args_marker)) break;
    } else {
        const msg = "no args array";
        __convex_error_set(ptrOf(msg), @intCast(msg.len));
        return 1;
    }
    // From the args array, find the first quoted string and emit it (with
    // its surrounding quotes) as the result — plain strings, no escapes.
    const from = start + args_marker.len;
    var i = from;
    while (i < input.len and input[i] != '"') : (i += 1) {}
    if (i >= input.len) {
        const msg = "expected a string argument";
        __convex_error_set(ptrOf(msg), @intCast(msg.len));
        return 1;
    }
    const open_q = i;
    i += 1;
    while (i < input.len and input[i] != '"') : (i += 1) {}
    if (i >= input.len) {
        const msg = "expected a string argument";
        __convex_error_set(ptrOf(msg), @intCast(msg.len));
        return 1;
    }
    __convex_output_set(ptrOf(input[open_q .. i + 1]), @intCast(i + 1 - open_q));
    return 0;
}

/// WASI reactor entry point: called once by the host before dispatch.
/// The Convex runner calls `_initialize` if the module exports it
/// (TypedFunc<(), ()>); a no-op here because Zig needs no runtime init.
export fn _initialize() void {}

/// Module analyzer entry: reports the function descriptors via output.
export fn __convex_functions() i32 {
    const funcs = "[{\"name\":\"echo\",\"type\":\"query\"}]";
    __convex_output_set(ptrOf(funcs), @intCast(funcs.len));
    return 0;
}
