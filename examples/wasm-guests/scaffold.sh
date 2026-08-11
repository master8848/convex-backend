#!/usr/bin/env bash
# Scaffold a new Convex WASM guest project from the example templates.
#
#   ./scaffold.sh <language> <project-name>   # rust|go|c|cpp
#   ./scaffold.sh --list                      # list available templates
#
# Run from examples/wasm-guests/ (or anywhere; the new project lands in
# ./<project-name>/ with the ABI imports/exports pre-wired).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "${1:-}" in
  --list|-l)
    echo "available templates: rust, go, c, cpp  (dart/kotlin: in progress)"
    exit 0
    ;;
  rust|go|c|cpp) LANG="$1" ;;
  *)
    echo "usage: $0 <rust|go|c|cpp> <project-name>   (or: $0 --list)" >&2
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
