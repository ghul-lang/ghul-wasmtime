# ghul-wasmtime: the wasmtime host for ghūl programs

Runs a ghūl program compiled with `--target wasm` under
[wasmtime](https://wasmtime.dev), replacing the .mjs loader for hosts
without Node. Shipped prebuilt (the ghul-llvm release pattern); no
ghūl user machine needs a Rust toolchain, Node, npm or Python. Product path decided 2026-10-14:
shipped prebuilt (the ghul-llvm release pattern); no user machine needs a
Rust toolchain, Node, npm or Python.

Status: runs the wasm-conformance sample byte-identically (22/22 of the
tests tried, 2026-10-14, wasmtime crate 49), the exception tests, and
the compiler building itself (a `--target wasm` ghūl compiler compiles
its own sources under this host). Real: js-string builtins,
write, clocks, math, bigint, exit, args, env, real file system, stdin,
identity_hash. Not yet: `host.throw` (exceptions — needs the module-side
`throw` decision), `schedule_timer`/`queue_microtask` (async).

## Getting it

Download the binary for your platform from this repository's GitHub
releases — one static executable, no runtime to install. The `ghul`
command-line tool fetches it the same way it fetches the other tools
when a project builds for the wasm target without Node.

## Building it

Developers only: `cargo build --release` (wasmtime 49, num-bigint,
num-integer) produces `target/release/ghul-wasmtime`.

Usage: `ghul-wasmtime <module.wasm> [program args...]`; program output
on stdout, host banners on stderr.

identity_hash relies on wasmtime's GC being non-moving: the raw GC index
of an object keys the hash table. If wasmtime's collector ever moves
objects this needs a redesign (a core-side hash slot is the alternative).

## Releases

The [releases page](https://github.com/ghul-lang/ghul-wasmtime/releases)
carries a prebuilt `ghul-wasmtime` for x86-64 Linux. Download it, mark it
executable, and run a wasm-target program through it:

```sh
ghul-wasmtime program.wasm [program arguments...]
```

Requirements and limits of the v0.1.0 build: glibc 2.34 or later
(Ubuntu 22.04+, Debian 12+); dynamically linked against libc, libm and
libgcc_s only. Asynchronous programs are not yet supported and suspend
rather than running. `--input-file <virtual>=<real>` seeds the program's
input files, and `--no-file-system` gives it an empty in-memory one, in
both loader modes.
