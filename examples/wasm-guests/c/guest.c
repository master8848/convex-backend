// A fixture C guest module implementing the Convex WASM ABI, used by the
// wasm_runner integration tests.
//
// Freestanding C: no libc, no wasi-libc, no headers. This is the same ABI a
// C/C++ game engine (or any language that links C) can implement. Built with:
//
//   clang --target=wasm32-wasip1 -O3 -nostdlib -Wl,--no-entry -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined -o c_guest.wasm guest.c
//
// The wasm32-wasip1 target is supported by stock clang/LLVM; `wasm-ld`
// handles linking. `--allow-undefined` lets the `env` imports resolve at
// instantiation time.

typedef unsigned int u32;
typedef int i32;
typedef unsigned long long u64;
typedef long long i64;

// Host functions (module "env"), matching wasm_runner/src/abi.rs.
__attribute__((import_module("env"), import_name("__convex_input_length")))
extern i32 input_length(void);
__attribute__((import_module("env"), import_name("__convex_input_load")))
extern void input_load(i32 offset, i32 dest, i32 length);
__attribute__((import_module("env"), import_name("__convex_output_set")))
extern void output_set(i32 ptr, i32 length);
__attribute__((import_module("env"), import_name("__convex_error_set")))
extern void error_set(i32 ptr, i32 length);
__attribute__((import_module("env"), import_name("__convex_log")))
extern void convex_log(i32 ptr, i32 length);

// Scratch buffer for the input payload. The host-alloc pattern means the
// guest only ever reads from its own memory.
#define INPUT_CAPACITY 65536
static unsigned char input_buffer[INPUT_CAPACITY];

// A tiny byte search: find `needle` inside `haystack`.
static i32 find_substring(const unsigned char *haystack, u32 haystack_len,
                          const char *needle) {
  u32 needle_len = 0;
  while (needle[needle_len] != 0) {
    needle_len++;
  }
  if (needle_len == 0 || needle_len > haystack_len) {
    return -1;
  }
  for (u32 i = 0; i + needle_len <= haystack_len; i++) {
    u32 j = 0;
    while (j < needle_len && haystack[i + j] == (unsigned char)needle[j]) {
      j++;
    }
    if (j == needle_len) {
      return (i32)i;
    }
  }
  return -1;
}

// Extract the first JSON string token (between the first two quotes) and emit
// it as a JSON string literal (with surrounding quotes) into `out`, returning
// the emitted length, or -1.
static i32 extract_first_string(const unsigned char *buf, u32 buf_len,
                                unsigned char *out) {
  u32 start = 0;
  while (start < buf_len && buf[start] != '"') {
    start++;
  }
  if (start >= buf_len) {
    return -1;
  }
  start++;
  u32 end = start;
  while (end < buf_len && buf[end] != '"') {
    end++;
  }
  if (end >= buf_len) {
    return -1;
  }
  u32 len = end - start;
  if (len > 4094) {
    return -1;
  }
  // Note: only handles plain strings, no escape sequences. The quotes make
  // the emitted payload valid JSON, as the host requires.
  out[0] = '"';
  for (u32 i = 0; i < len; i++) {
    out[i + 1] = buf[start + i];
  }
  out[len + 1] = '"';
  return (i32)(len + 2);
}

// The dispatcher. Returns 0 on success, non-zero on error.
__attribute__((export_name("__convex_run")))
i32 __convex_run(void) {
  i32 len = input_length();
  if (len <= 0 || (u32)len > INPUT_CAPACITY) {
    return 1;
  }
  input_load(0, (i32)(u32)&input_buffer[0], len);
  const unsigned char *input = input_buffer;

  // The host sends `{"function": "<name>", "args": [...]}` (see wasm_runner
  // src/lib.rs), so match the exact `"function": "echo"` byte sequence.
  if (find_substring(input, (u32)len, "\"function\": \"echo\"") >= 0) {
    // echo: return the first argument, which is a JSON string.
    static unsigned char out_buffer[4096];
    i32 args_idx = find_substring(input, (u32)len, "\"args\": [");
    i32 out_len =
        args_idx < 0
            ? -1
            : extract_first_string(input + args_idx + 9, (u32)len - (u32)args_idx - 9,
                                   out_buffer);
    if (out_len < 0) {
      error_set((i32)(u32)"expected a string argument", 26);
      return 1;
    }
    output_set((i32)(u32)&out_buffer[0], out_len);
    return 0;
  }

  error_set((i32)(u32)"unknown function", 15);
  return 1;
}

// The list of functions in the module, returned as JSON.
__attribute__((export_name("__convex_functions")))
i32 __convex_functions(void) {
  static const char functions[] = "[{\"name\":\"echo\",\"type\":\"query\"}]";
  output_set((i32)(u32)&functions[0], sizeof(functions) - 1);
  return 0;
}
