// A C++ guest module implementing the Convex WASM ABI, mirroring the C
// example (guest.c) with the freestanding-C++ rules:
//
//   - no libc++, no exceptions (-fno-exceptions), no RTTI (-fno-rtti)
//   - no dynamic static initialization (no guard variables): only POD
//     globals and constexpr/const-initialized statics
//   - the host-alloc ABI means the guest never touches host memory and
//     never exports an allocator
//
// Build (LLVM clang++; Apple clang lacks wasm targets):
//
//   clang++ --target=wasm32-wasip1 -O3 -nostdlib -fno-exceptions -fno-rtti \
//           -fno-threadsafe-statics -Wl,--no-entry \
//           -Wl,--export=__convex_run -Wl,--export=__convex_functions \
//           -Wl,--allow-undefined -o guest.wasm guest.cpp
//
// C++ features that survive -nostdlib: namespaces, classes (POD), member
// functions, constexpr, templates, operator overloading, references.
//
// No headers at all (not even <cstdint>): stock LLVM clang++ for
// wasm32-wasip1 has no libc++ include dir without a wasi-sdk sysroot, so the
// guest uses built-in integer types (fixed-size on wasm32: int = 32-bit,
// long long = 64-bit).

using u32 = unsigned int;
using i32 = int;
using u64 = unsigned long long;
using i64 = long long;

namespace convex {
namespace abi {
// Host functions (module "env"), matching wasm_runner/src/abi.rs.
extern "C" {
__attribute__((import_module("env"), import_name("__convex_input_length")))
i32 input_length();
__attribute__((import_module("env"), import_name("__convex_input_load")))
void input_load(i32 offset, i32 dest, i32 length);
__attribute__((import_module("env"), import_name("__convex_output_set")))
void output_set(i32 ptr, i32 length);
__attribute__((import_module("env"), import_name("__convex_error_set")))
void error_set(i32 ptr, i32 length);
__attribute__((import_module("env"), import_name("__convex_log")))
void log(i32 ptr, i32 length);
}  // extern "C"
}  // namespace abi

// Scratch buffer for the input payload (POD global: no dynamic init).
constexpr u32 kInputCapacity = 65536;
alignas(8) unsigned char input_buffer[kInputCapacity];

// A tiny constexpr-friendly byte scanner.
struct ByteScanner {
  const unsigned char* data;
  u32 len;

  constexpr ByteScanner(const unsigned char* d, u32 n) : data(d), len(n) {}

  // Index of `needle` (NUL-terminated) in the buffer, or -1.
  i32 find(const char* needle) const {
    u32 needle_len = 0;
    while (needle[needle_len] != 0) needle_len++;
    if (needle_len == 0 || needle_len > len) return -1;
    for (u32 i = 0; i + needle_len <= len; i++) {
      u32 j = 0;
      while (j < needle_len && data[i + j] == (unsigned char)needle[j]) j++;
      if (j == needle_len) return static_cast<i32>(i);
    }
    return -1;
  }
};

// Extracts the first JSON string token (between the first two quotes) and
// emits it quoted (so the payload stays valid JSON) into `out`, returning
// the emitted length or -1. Plain strings only, no escape sequences.
template <u32 OutCapacity>
i32 extract_first_string(const ByteScanner& s, unsigned char (&out)[OutCapacity]) {
  const i32 start = s.find("\"");
  if (start < 0) return -1;
  const u32 content_begin = static_cast<u32>(start) + 1;
  const u32 content_end = content_begin;
  // find closing quote
  u32 end = content_begin;
  while (end < s.len && s.data[end] != '"') end++;
  if (end >= s.len) return -1;
  const u32 content_len = end - content_begin;
  if (content_len + 2 > OutCapacity) return -1;
  out[0] = '"';
  for (u32 i = 0; i < content_len; i++) out[i + 1] = s.data[content_begin + i];
  out[content_len + 1] = '"';
  return static_cast<i32>(content_len + 2);
}

// The dispatcher. Returns 0 on success, non-zero on error.
extern "C" __attribute__((export_name("__convex_run")))
i32 __convex_run() {
  const i32 len = abi::input_length();
  if (len <= 0 || static_cast<u32>(len) > kInputCapacity) return 1;
  abi::input_load(0, reinterpret_cast<i32>(input_buffer), len);
  const ByteScanner input(input_buffer, static_cast<u32>(len));

  if (input.find("\"function\": \"echo\"") >= 0) {
    static unsigned char out_buffer[4096];  // const-init POD: safe
    const i32 args_idx = input.find("\"args\": [");
    const i32 out_len = args_idx < 0
        ? -1
        : extract_first_string(
              ByteScanner(input.data + args_idx + 9, input.len - static_cast<u32>(args_idx) - 9),
              out_buffer);
    if (out_len < 0) {
      static const char err[] = "expected a string argument";
      abi::error_set(reinterpret_cast<i32>(err), sizeof(err) - 1);
      return 1;
    }
    abi::output_set(reinterpret_cast<i32>(out_buffer), out_len);
    return 0;
  }

  static const char unknown[] = "unknown function";
  abi::error_set(reinterpret_cast<i32>(unknown), sizeof(unknown) - 1);
  return 1;
}

// The list of functions in the module, returned as JSON.
extern "C" __attribute__((export_name("__convex_functions")))
i32 __convex_functions() {
  static const char functions[] = "[{\"name\":\"echo\",\"type\":\"query\"}]";
  abi::output_set(reinterpret_cast<i32>(functions), sizeof(functions) - 1);
  return 0;
}
}  // namespace convex
