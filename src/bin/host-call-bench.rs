// Times one imported host call (argument_count, no arguments, one i32
// result) called 10 million times from a wasm loop, under host
// registration shapes:
//
//   match-dynamic   one closure per import whose body matches the op name,
//                   values crossing as &[Val] (the shape the host shipped in)
//   op-dynamic      one dedicated closure per op, still dynamic &[Val]
//   op-typed        the guest's _start called through TypedFunc (the guest
//                   call typed; the host closure stays dynamic, since
//                   TypedFunc types the call into the guest, not the host
//                   function's own ABI)
//
// Usage: host-call-bench <module.wasm>
use std::time::Instant;

use wasmtime::{Config, Engine, Func, FuncType, Instance, Module, Store, TypedFunc, Val};

const CALLS: i32 = 10_000_000;

fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_exceptions(true);
    Engine::new(&config).unwrap()
}

fn run_match_dynamic(engine: &Engine, path: &str) -> (u128, u128) {
    let module = Module::from_file(engine, path).unwrap();
    let mut store = Store::new(engine, ());
    let mut imports = Vec::new();

    for imp in module.imports() {
        let name = imp.name().to_string();
        let ty: FuncType = imp.ty().unwrap_func().clone();
        let f = Func::new(&mut store, ty.clone(), move |_caller, _args, outs| {
            match name.as_str() {
                "argument_count" => outs[0] = Val::I32(CALLS),
                other => panic!("unhandled {other}"),
            }
            Ok(())
        });
        imports.push(f.into());
    }

    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let start = instance.get_func(&mut store, "_start").unwrap();
    let started = Instant::now();
    start.call(&mut store, &[], &mut []).unwrap();
    let elapsed = started.elapsed().as_nanos();
    (elapsed / CALLS as u128, elapsed)
}

fn run_op_dynamic(engine: &Engine, path: &str) -> (u128, u128) {
    let module = Module::from_file(engine, path).unwrap();
    let mut store = Store::new(engine, ());
    let mut imports = Vec::new();

    for imp in module.imports() {
        let ty: FuncType = imp.ty().unwrap_func().clone();
        let f = Func::new(&mut store, ty, |_caller, _args, outs| {
            outs[0] = Val::I32(CALLS);
            Ok(())
        });
        imports.push(f.into());
    }

    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let start = instance.get_func(&mut store, "_start").unwrap();
    let started = Instant::now();
    start.call(&mut store, &[], &mut []).unwrap();
    let elapsed = started.elapsed().as_nanos();
    (elapsed / CALLS as u128, elapsed)
}

// TypedFunc types the guest call; _start returns nothing, so call it as
// () -> () and time the whole loop against its own inner calls.
fn run_op_typed(engine: &Engine, path: &str) -> (u128, u128) {
    let module = Module::from_file(engine, path).unwrap();
    let mut store = Store::new(engine, ());
    let mut imports = Vec::new();

    for imp in module.imports() {
        let ty: FuncType = imp.ty().unwrap_func().clone();
        let f = Func::new(&mut store, ty, |_caller, _args, outs| {
            outs[0] = Val::I32(CALLS);
            Ok(())
        });
        imports.push(f.into());
    }

    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let start: TypedFunc<(), ()> = instance.get_typed_func::<(), ()>(&mut store, "_start").unwrap();
    let started = Instant::now();
    start.call(&mut store, ()).unwrap();
    let elapsed = started.elapsed().as_nanos();
    (elapsed / CALLS as u128, elapsed)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: host-call-bench <module.wasm>");
    let engine = engine();

    let (per, total) = run_match_dynamic(&engine, &path);
    println!("match-dynamic: {per} ns/call ({} ms total)", total / 1_000_000);

    let (per, total) = run_op_dynamic(&engine, &path);
    println!("op-dynamic:    {per} ns/call ({} ms total)", total / 1_000_000);

    let (per, total) = run_op_typed(&engine, &path);
    println!("op-typed:      {per} ns/call ({} ms total)", total / 1_000_000);
}
