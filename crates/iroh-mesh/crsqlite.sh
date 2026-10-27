#!/usr/bin/env bash
# Builds cr-sqlite as one static object, so imesh needs no crsqlite.so or crsqlite.dll at run time.
# A static musl binary cannot dlopen, and Corrosion ships no Windows build.
# Usage: crsqlite.sh <rust target> <output .a>
# Needs git, rustup, a libclang older than 21 for bindgen 0.68, and per target:
#   musl: zig (cc, ld.lld -r, ar) and llvm-objcopy
#   x86_64-pc-windows-gnu: mingw-w64 (x86_64-w64-mingw32-gcc, -ld -r, -objcopy, -ar)
set -euo pipefail

TARGET=$1
OUT=$2
# The cr-sqlite build that Corrosion ef243dd ships as crsqlite-linux-*.so ("prebuild-test.main-dbversion-cache").
CRSQLITE_REV=3133cacac0177ff11a2022a5ebd08f17749559f5
EMBEDDED_REV=aba562843b26642e2242f8e3388b83a1a2625031
TOOLCHAIN=nightly-2023-10-05
case $TARGET in
aarch64-unknown-linux-musl) ZT=aarch64-linux-musl ;;
x86_64-unknown-linux-musl) ZT=x86_64-linux-musl ;;
x86_64-pc-windows-gnu) MW=x86_64-w64-mingw32 ;;
*) echo "crsqlite.sh: unsupported target $TARGET" >&2; exit 1 ;;
esac

# Outside the imesh tree: cargo would otherwise take imesh as the workspace of the inner build.
W=${XDG_CACHE_HOME:-$HOME/.cache}/imesh-crsqlite/$CRSQLITE_REV
if [ ! -d "$W/.git" ]; then
	rm -rf "$W"
	git clone -q https://github.com/superfly/cr-sqlite.git "$W"
	git -C "$W" checkout -q $CRSQLITE_REV
	rmdir "$W/core/rs/sqlite-rs-embedded" 2>/dev/null || true
	git clone -q https://github.com/vlcn-io/sqlite-rs-embedded.git "$W/core/rs/sqlite-rs-embedded"
	git -C "$W/core/rs/sqlite-rs-embedded" checkout -q $EMBEDDED_REV
fi
rustup toolchain list | grep -q "^$TOOLCHAIN" || rustup toolchain install $TOOLCHAIN --profile minimal
rustup target list --installed --toolchain $TOOLCHAIN | grep -q "$TARGET" || rustup target add --toolchain $TOOLCHAIN "$TARGET"

if [ -n "${MW:-}" ]; then
	INC=$(dirname "$(command -v $MW-gcc)")/../opt/mingw-w64/toolchain-x86_64/$MW/include
	export BINDGEN_EXTRA_CLANG_ARGS="--target=$TARGET -isystem $INC"
	CC="$MW-gcc" CFLAGS="-O2 -std=c99" BUILD=build
	LD_R="$MW-ld -r" OBJCOPY="$MW-objcopy" AR="$MW-ar"
else
	Z=$(zig env | sed -n 's/.*\.lib_dir = "\(.*\)".*/\1/p')
	A=${ZT%%-*}
	export BINDGEN_EXTRA_CLANG_ARGS="-isystem $Z/libc/include/$ZT -isystem $Z/libc/include/generic-musl -isystem $Z/libc/include/$A-linux-any -isystem $Z/libc/include/any-linux-any"
	CC="zig cc -target $ZT" CFLAGS="-O2 -std=c99 -fPIC" BUILD=zigbuild
	LD_R="zig ld.lld -r" OBJCOPY=$(command -v llvm-objcopy || ls -d /opt/homebrew/opt/llvm/bin/llvm-objcopy /opt/homebrew/opt/llvm@*/bin/llvm-objcopy /usr/lib/llvm-*/bin/llvm-objcopy 2>/dev/null | tail -1 || true) AR="zig ar"
fi
if [ -z "${LIBCLANG_PATH:-}" ]; then
	for d in /opt/homebrew/opt/llvm@20/lib /opt/homebrew/opt/llvm@19/lib /usr/lib/llvm-20/lib /usr/lib/llvm-19/lib /usr/lib/llvm-18/lib; do
		[ -d "$d" ] && export LIBCLANG_PATH=$d && break
	done
fi
export CRSQLITE_COMMIT_SHA=$CRSQLITE_REV
(cd "$W/core/rs/bundle_static" && cargo +$TOOLCHAIN $BUILD -q --release --target "$TARGET" --features loadable_extension)

B=$W/obj-$TARGET
rm -rf "$B" && mkdir -p "$B"
for f in crsqlite changes-vtab ext-data; do
	$CC $CFLAGS -I"$W/core/src" -I"$W/core/src/sqlite" -c "$W/core/src/$f.c" -o "$B/$f.o"
done
$LD_R -o "$B/all.o" "$B"/crsqlite.o "$B"/changes-vtab.o "$B"/ext-data.o \
	--whole-archive "$W/core/rs/bundle_static/target/$TARGET/release/libcrsql_bundle_static.a"
# Only the entry point stays global; the Rust allocator, panic handler, and core copies become local.
# LLVM bitcode is build metadata. MinGW can place it outside the PE image and Windows
# then refuses to load the executable. Keep only the compiled code and data.
"$OBJCOPY" --remove-section=.llvmbc --remove-section=.llvmcmd --keep-global-symbol=sqlite3_crsqlite_init --strip-debug "$B/all.o" "$B/crsqlite-static.o"
mkdir -p "$(dirname "$OUT")" && rm -f "$OUT"
$AR rcs "$OUT" "$B/crsqlite-static.o"
