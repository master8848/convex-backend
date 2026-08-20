#!/usr/bin/env bash
# Scaffold a new Convex WASM guest project from the example templates.
#
#   ./scaffold.sh <language> <project-name>   # rust|go|c|cpp|kotlin
#   ./scaffold.sh --list                      # list available templates
#
# Run from examples/wasm-guests/ (or anywhere; the new project lands in
# ./<project-name>/ with the ABI imports/exports pre-wired).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
  --list|-l)
    echo "available templates: rust, go, c, cpp, kotlin  (zig: via make, dart: in progress)"
    exit 0
    ;;
  rust|go|c|cpp|kotlin) LANG="$1" ;;
  *)
    echo "usage: $0 <rust|go|c|cpp|kotlin> <project-name>   (or: $0 --list)" >&2
    exit 1
    ;;
esac

NAME="${2:-}"
if [[ -z "$NAME" ]]; then
  echo "error: missing project name" >&2
  exit 1
fi
if [[ ! "$NAME" =~ ^[a-zA-Z0-9_]+$ ]]; then
  echo "error: project name must be alphanumeric/underscores only" >&2
  exit 1
fi
if [[ -e "$NAME" ]]; then
  echo "error: $NAME already exists" >&2
  exit 1
fi

case "$LANG" in
  kotlin)
    mkdir -p "$NAME/src/wasmWasiMain/kotlin/convex/sdk"
    cp "$HERE/kotlin/src/wasmWasiMain/kotlin/Guest.kt" "$NAME/src/wasmWasiMain/kotlin/Guest.kt" 2>/dev/null || cp "$HERE/../crates/wasm_runner/tests/fixtures/kotlin_guest/src/wasmWasiMain/kotlin/Guest.kt" "$NAME/src/wasmWasiMain/kotlin/Guest.kt"
    cp "$HERE/kotlin/build.gradle.kts" "$NAME/build.gradle.kts" 2>/dev/null || cp "$HERE/../crates/wasm_runner/tests/fixtures/kotlin_guest/build.gradle.kts" "$NAME/build.gradle.kts"
    cp "$HERE/kotlin/settings.gradle.kts" "$NAME/settings.gradle.kts" 2>/dev/null || cp "$HERE/../crates/wasm_runner/tests/fixtures/kotlin_guest/settings.gradle.kts" "$NAME/settings.gradle.kts"
    # SDK vendoring — keep hermetic without Maven publish
    if [[ -f "$HERE/kotlin/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt" ]]; then
      mkdir -p "$NAME/src/wasmWasiMain/kotlin/convex/sdk"
      cp "$HERE/kotlin/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt" "$NAME/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt"
    elif [[ -f "$HERE/../crates/convex_sdk_kotlin/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt" ]]; then
      mkdir -p "$NAME/src/wasmWasiMain/kotlin/convex/sdk"
      cp "$HERE/../crates/convex_sdk_kotlin/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt" "$NAME/src/wasmWasiMain/kotlin/convex/sdk/ConvexSdk.kt"
    fi
    # Replace project name in settings.gradle.kts if present
    if [[ -f "$NAME/settings.gradle.kts" ]]; then
      sed -i.bak 's/rootProject.name = ".*"/rootProject.name = "'"$NAME"'"/' "$NAME/settings.gradle.kts" && rm -f "$NAME/settings.gradle.kts.bak"
    fi
    cp "$HERE/kotlin/.gitignore" "$NAME/.gitignore" 2>/dev/null || cp "$HERE/../crates/wasm_runner/tests/fixtures/kotlin_guest/.gitignore" "$NAME/.gitignore" 2>/dev/null || true
    echo "scaffolded kotlin guest in ./$NAME"
    echo "  build:  (cd $NAME && gradle build --console=plain && gradle copyWasm --console=plain)"
    ;;
  rust)
    mkdir -p "$NAME/src"
    cp "$HERE/rust/src/lib.rs" "$NAME/src/lib.rs"
    # Replace only the package name; the template's convex_sdk path
    # (../../../crates/convex_sdk) stays valid for projects scaffolded
    # from examples/wasm-guests/.
    sed -e 's/^name = ".*"/name = "'"$NAME"'"/' \
        "$HERE/rust/Cargo.toml" > "$NAME/Cargo.toml"
    cp "$HERE/rust/.gitignore" "$NAME/.gitignore" 2>/dev/null || true
    echo "scaffolded rust guest in ./$NAME"
    echo "  build:  (cd $NAME && cargo build --target wasm32-wasip1 --release)"
    ;;
  go)
    mkdir -p "$NAME"
    cp "$HERE/go/main.go" "$NAME/main.go"
    cp "$HERE/go/go.mod" "$NAME/go.mod"
    sed -i.bak 's/^module .*/module '"$NAME"'/' "$NAME/go.mod" && rm -f "$NAME/go.mod.bak"
    cp "$HERE/go/.gitignore" "$NAME/.gitignore" 2>/dev/null || true
    echo "scaffolded go guest in ./$NAME"
    echo "  build:  (cd $NAME && GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -o $NAME.wasm .)"
    ;;
  c|cpp)
    mkdir -p "$NAME"
    ext=$([[ "$LANG" == cpp ]] && echo cpp || echo c)
    cp "$HERE/$LANG/guest.$ext" "$NAME/guest.$ext"
    echo "scaffolded $LANG guest in ./$NAME"
    echo "  build:  clang$([ "$LANG" = cpp ] && echo ++) --target=wasm32-wasip1 -O3 -nostdlib -fno-exceptions -Wl,--no-entry \\"
    echo "          -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined \\"
    echo "          -o $NAME.wasm guest.$ext   (add -fno-rtti -fno-threadsafe-statics for cpp)"
    ;;
esac
