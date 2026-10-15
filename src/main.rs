use std::time::{Instant, SystemTime, UNIX_EPOCH};

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};
use wasmtime::{ArrayRef, Config, Engine, ExternRef, Func, FuncType, Instance, Module, Rooted, Store, Val, ValType};

// Host state, reachable from every host function through Caller::data.
struct HostState {
    stdin: Option<Vec<u8>>,
    stdin_pos: usize,
    started: Instant,
    args: Vec<String>,
    env: Vec<(String, String)>,
    entries: Vec<String>,
    input_files: Vec<(String, Vec<u8>)>,
    real_fs: bool,
    last_error: i32,
    identities: std::collections::HashMap<u32, i32>,
    next_identity: u32,
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: wasmtime-host <module.wasm>");

    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_exceptions(true);

    // The copying collector moves objects, which would invalidate the raw
    // GC indices the identity table keys on. The deferred
    // reference-counting collector never moves an object, so an object's
    // index is stable for as long as it lives - which the table guarantees
    // by rooting every hashed object. Cycles leak until the store drops;
    // revisit if a long-running program shows that.
    config.collector(wasmtime::Collector::DeferredReferenceCounting);
    let engine = Engine::new(&config).unwrap();
    let module = Module::from_file(&engine, &path).unwrap();
    // --input-file <virtual>=<real> seeds the input_file_* surface, the
    // loader's options.files; everything else is the program's argv.
    let mut input_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut real_fs = true;
    let mut args: Vec<String> = Vec::new();

    let mut argv = std::env::args().skip(2);
    while let Some(arg) = argv.next() {
        if arg == "--input-file" {
            let spec = argv.next().unwrap_or_else(|| panic!("--input-file needs <virtual>=<real>"));
            let (virtual_path, real_path) = spec.split_once('=').unwrap_or_else(|| panic!("--input-file needs <virtual>=<real>, got {spec}"));
            let bytes = std::fs::read(real_path).unwrap_or_else(|e| panic!("--input-file cannot read {real_path}: {e}"));
            input_files.push((virtual_path.to_string(), bytes));
        } else if arg == "--no-file-system" {
            real_fs = false;
        } else {
            args.push(arg);
        }
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    let mut store = Store::new(
        &engine,
        HostState {
            stdin: None,
            stdin_pos: 0,
            started: Instant::now(),
            args,
            env,
            entries: Vec::new(),
            input_files,
            real_fs,
            last_error: 0,
            identities: std::collections::HashMap::new(),
            next_identity: 0,
        },
    );

    let mut imports: Vec<wasmtime::Extern> = Vec::new();

    for imp in module.imports() {
        let name = imp.name().to_string();
        let module_name = imp.module().to_string();
        let ty: FuncType = imp.ty().unwrap_func().clone();

        let f = match module_name.as_str() {
            "wasm:js-string" => js_string_fn(&mut store, &name, &ty),
            "ghul:host" => host_fn(&mut store, &name, &ty),
            other => panic!("unknown import module {other}"),
        };

        imports.push(f);
    }

    let instance = Instance::new(&mut store, &module, &imports).expect("instantiate");
    let start = instance.get_func(&mut store, "_start").expect("_start");

    // The host stays silent unless it is asked to trace: a program's output
    // is its own, and timings on standard error are diagnostics, not
    // something every run should show.
    let trace = std::env::var_os("GHUL_WASMTIME_TRACE").is_some();

    if trace {
        eprintln!("[host] instantiated in {:?}", store.data().started.elapsed());
    }

    let run_start = Instant::now();
    let result_count = start.ty(&store).results().len();
    let mut result: Vec<Val> = vec![Val::I32(0); result_count];
    start.call(&mut store, &[], &mut result).expect("_start ran");

    if trace {
        if let Some(Val::I32(code)) = result.first() {
            eprintln!("[host] _start result: {code}");
        }
        eprintln!("[host] _start finished in {:?}", run_start.elapsed());
    }
}

// ---- js-string builtins -------------------------------------------------

fn js_string_fn(store: &mut Store<HostState>, name: &str, ty: &FuncType) -> wasmtime::Extern {
    let name = name.to_string();

    let f = Func::new(store, ty.clone(), move |mut caller, args, outs| {
        match name.as_str() {
            // (array-of-i16, start, len) -> externref string
            "fromCharCodeArray" => {
                let arr = as_array(&caller, &args[0]).expect("fromCharCodeArray: not an array");
                let start = unwrap_i32(&args[1]) as u32;
                let len = unwrap_i32(&args[2]) as u32;

                let mut units: Vec<u16> = Vec::with_capacity(len as usize);
                for i in start..start + len {
                    let v = arr.get(&mut caller, i).expect("array get");
                    units.push(unwrap_i32(&v) as u16);
                }

                let text = String::from_utf16(&units).unwrap_or_default();
                outs[0] = new_text(&mut caller, &text);
                Ok(())
            }
            // (externref string) -> i32
            "length" => {
                outs[0] = Val::I32(text_of(&caller, &args[0]).encode_utf16().count() as i32);
                Ok(())
            }
            // (externref string, array-of-i16, start) -> i32 written
            "intoCharCodeArray" => {
                let text = text_of(&caller, &args[0]);
                let arr = as_array(&caller, &args[1]).expect("intoCharCodeArray: not an array");
                let start = unwrap_i32(&args[2]) as u32;

                let units: Vec<u16> = text.encode_utf16().collect();
                for (i, u) in units.iter().enumerate() {
                    arr.set(&mut caller, start + i as u32, Val::I32(*u as i32)).expect("array set");
                }

                outs[0] = Val::I32(units.len() as i32);
                Ok(())
            }
            other => panic!("unhandled js-string builtin {other}"),
        }
    });

    wasmtime::Extern::Func(f)
}

// ---- ghul:host ----------------------------------------------------------

fn host_fn(store: &mut Store<HostState>, name: &str, ty: &FuncType) -> wasmtime::Extern {
    let name = name.to_string();
    let result_types: Vec<ValType> = ty.results().map(|t| t.clone()).collect();

    let f = Func::new(store, ty.clone(), move |mut caller, args, outs| {
        match name.as_str() {
            "write" => {
                let text = text_of(&caller, &args[0]);
                if unwrap_i32(&args[1]) == 2 {
                    eprint!("{text}");
                } else {
                    print!("{text}");
                }
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(())
            }
            "monotonic_milliseconds" => {
                outs[0] = Val::from(caller.data().started.elapsed().as_secs_f64());
                Ok(())
            }
            "wall_clock_milliseconds" => {
                let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
                outs[0] = Val::from(ms);
                Ok(())
            }
            "random_u32" => {
                outs[0] = Val::I32(rand_u32() as i32);
                Ok(())
            }
            "exit" => {
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(unwrap_i32(&args[0]));
            }
            "argument_count" => {
                outs[0] = Val::I32(caller.data().args.len() as i32);
                Ok(())
            }
            "argument" => {
                let i = unwrap_i32(&args[0]);
                let name = caller.data().args.get(i as usize).cloned().unwrap_or_default();
                outs[0] = new_text(&mut caller, &name);
                Ok(())
            }
            "environment_count" => {
                outs[0] = Val::I32(caller.data().env.len() as i32);
                Ok(())
            }
            "environment_name" => {
                let i = unwrap_i32(&args[0]);
                let name = caller.data().env.get(i as usize).map(|(k, _)| k.clone()).unwrap_or_default();
                outs[0] = new_text(&mut caller, &name);
                Ok(())
            }
            "environment_lookup" => {
                let want = text_of(&caller, &args[0]);
                let value = caller.data().env.iter().find(|(k, _)| *k == want).map(|(_, v)| v.clone());
                outs[0] = match value {
                    Some(v) => new_text(&mut caller, &v),
                    None => Val::ExternRef(None),
                };
                Ok(())
            }
            "input_file_count" => {
                outs[0] = Val::I32(caller.data().input_files.len() as i32);
                Ok(())
            }
            "input_file_path" => {
                let i = unwrap_i32(&args[0]);
                let path = caller.data().input_files.get(i as usize).map(|(p, _)| p.clone()).unwrap_or_default();
                outs[0] = new_text(&mut caller, &path);
                Ok(())
            }
            "input_file_content" => {
                // Content as text, each byte a char, as file_read_text reads.
                let i = unwrap_i32(&args[0]);
                let content: String = caller
                    .data()
                    .input_files
                    .get(i as usize)
                    .map(|(_, b)| b.iter().map(|&byte| byte as char).collect())
                    .unwrap_or_default();
                outs[0] = new_text(&mut caller, &content);
                Ok(())
            }
            "file_read_text" => {
                let path = text_of(&caller, &args[0]);
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let text: String = bytes.iter().map(|&b| b as char).collect();
                        caller.data_mut().last_error = 0;
                        outs[0] = new_text(&mut caller, &text);
                    }
                    Err(e) => {
                        caller.data_mut().last_error = error_code(&e, &path);
                        outs[0] = Val::ExternRef(None);
                    }
                }
                Ok(())
            }
            "stdin_peek" | "stdin_read" => {
                let state = caller.data_mut();
                fill_stdin(state);
                let text = String::from_utf8_lossy(state.stdin.as_deref().unwrap_or(&[])).into_owned();
                let at = state.stdin_pos;
                let units: Vec<u16> = text.encode_utf16().collect();
                let b = if at < units.len() {
                    if name == "stdin_read" {
                        state.stdin_pos += 1;
                    }
                    units[at] as i32
                } else {
                    -1
                };
                outs[0] = Val::I32(b);
                Ok(())
            }
            "stdin_read_line" => {
                let line = {
                    let state = caller.data_mut();
                    fill_stdin(state);
                    let text = String::from_utf8_lossy(state.stdin.as_deref().unwrap_or(&[])).into_owned();
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let at = state.stdin_pos;

                    let mut end = None;
                    for i in at..units.len() {
                        if units[i] == 10 || units[i] == 13 {
                            end = Some(i);
                            break;
                        }
                    }

                    match end {
                        Some(e) => {
                            let line: String = String::from_utf16(&units[at..e]).unwrap_or_default();
                            let crlf = units[e] == 13 && e + 1 < units.len() && units[e + 1] == 10;
                            state.stdin_pos = e + if crlf { 2 } else { 1 };
                            line
                        }
                        None => {
                            if at < units.len() {
                                state.stdin_pos = units.len();
                                String::from_utf16(&units[at..]).unwrap_or_default()
                            } else {
                                String::new()
                            }
                        }
                    }
                };

                if caller.data().stdin_pos > caller.data().stdin.as_ref().map(|b| b.len()).unwrap_or(0)
                    && line.is_empty()
                {
                    // Nothing left at all: the loader answers null.
                    outs[0] = Val::ExternRef(None);
                } else {
                    outs[0] = new_text(&mut caller, &line);
                }

                Ok(())
            }
            "stdin_read_to_end" => {
                let rest = {
                    let state = caller.data_mut();
                    fill_stdin(state);
                    let text = String::from_utf8_lossy(state.stdin.as_deref().unwrap_or(&[])).into_owned();
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let at = state.stdin_pos;
                    state.stdin_pos = units.len();
                    String::from_utf16(&units[at..]).unwrap_or_default()
                };
                outs[0] = new_text(&mut caller, &rest);
                Ok(())
            }
            "file_written" => {
                // In-memory-FS mode: the program's write arrives as
        // base64 text and lands in the real working directory, the
        // loader's Node default.
                let path = text_of(&caller, &args[0]);
                let content = text_of(&caller, &args[1]);
                let bytes = base64_decode(&content);
                let dest = std::path::Path::new(&path);
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(dest, &bytes) {
                    eprintln!("[host] file_written {path}: {e}");
                }
                Ok(())
            }
            "identity_hash" => {
                // Object identity = the GC's own hash of the object, mixed.
                // ref_hash is equal for every rooting of the same object;
                // under the non-moving collector selected above it is also
                // stable for the object's lifetime (under the default
                // copying collector it is not - objects move, and the hash
                // follows the move - which is why this collector choice is
                // load-bearing). A recycled slot colliding with a dead
                // object's hash is a hash collision, not a wrong answer.
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                if let Val::AnyRef(Some(a)) = &args[0] {
                    let _ = a.ref_hash(&caller, &mut hasher);
                }
                let slot = std::hash::Hasher::finish(&hasher);
                outs[0] = Val::I32(((slot as u32).wrapping_mul(0x9E37_79B1)) as i32);
                Ok(())
            }
            "file_written" => {
                // In-memory-FS mode: the program's write arrives as
        // base64 text and lands in the real working directory, the
        // loader's Node default.
                let path = text_of(&caller, &args[0]);
                let content = text_of(&caller, &args[1]);
                let bytes = base64_decode(&content);
                let dest = std::path::Path::new(&path);
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(dest, &bytes) {
                    eprintln!("[host] file_written {path}: {e}");
                }
                Ok(())
            }
            other => {
                if let Some(m) = other.strip_prefix("math_") {
                    let x = unwrap_f64(&args[0]);
                    let y = if args.len() > 1 { unwrap_f64(&args[1]) } else { 0.0 };
                    let v = match m {
                        "sqrt" => x.sqrt(),
                        "cbrt" => x.cbrt(),
                        "floor" => x.floor(),
                        "ceiling" => x.ceil(),
                        "truncate" => x.trunc(),
                        "sin" => x.sin(),
                        "cos" => x.cos(),
                        "tan" => x.tan(),
                        "asin" => x.asin(),
                        "acos" => x.acos(),
                        "atan" => x.atan(),
                        "atan2" => x.atan2(y),
                        "sinh" => x.sinh(),
                        "cosh" => x.cosh(),
                        "tanh" => x.tanh(),
                        "exp" => x.exp(),
                        "log" => x.ln(),
                        "log10" => x.log10(),
                        "log2" => x.log2(),
                        "pow" => x.powf(y),
                        other => panic!("unhandled math op {other}"),
                    };
                    outs[0] = Val::from(v);
                    return Ok(());
                }

                if let Some(op) = other.strip_prefix("bigint_") {
                    return bigint(&mut caller, op, args, outs);
                }

                file_op(&mut caller, other, args, outs, &result_types)
            }
        }
    });

    wasmtime::Extern::Func(f)
}

// bigint values cross as externrefs holding a host BigInt, exactly as the
// .mjs loader holds JS BigInt behind them.
fn bigint(
    caller: &mut wasmtime::Caller<'_, HostState>,
    op: &str,
    args: &[Val],
    outs: &mut [Val],
) -> Result<(), wasmtime::Error> {
    let big = |caller: &wasmtime::Caller<'_, HostState>, v: &Val| -> BigInt {
        match v {
            Val::ExternRef(Some(e)) => {
                let data = e.data(caller).expect("externref data").expect("bigint present");
                if let Some(b) = data.downcast_ref::<BigInt>() {
                    b.clone()
                } else if let Some(s) = data.downcast_ref::<String>() {
                    // Numeric text reaches bigint ops directly (bigint_parse
                    // takes a string); read it rather than fail.
                    s.trim().parse::<BigInt>().unwrap_or_else(|_| BigInt::from(0))
                } else if let Some(i) = data.downcast_ref::<i64>() {
                    BigInt::from(*i)
                } else {
                    panic!("bigint externref of wrong type");
                }
            }
            Val::I64(v) => BigInt::from(*v),
            Val::I32(v) => BigInt::from(*v as i64),
            _ => BigInt::from(0),
        }
    };

    let big0 = big(caller, &args[0]);
    let big1 = if args.len() > 1 { big(caller, &args[1]) } else { BigInt::from(0) };

    let produce: BigInt = match op {
        "from_long" => BigInt::from(unwrap_i64(&args[0])),
        "from_ulong" => BigInt::from(unwrap_i64(&args[0]) as u64),
        "parse" => text_of(caller, &args[0]).trim().parse::<BigInt>().unwrap_or_else(|_| BigInt::from(0)),
        "from_double" => BigInt::from(unwrap_f64(&args[0]).trunc() as i64),
        "add" => &big0 + &big1,
        "subtract" => &big0 - &big1,
        "multiply" => &big0 * &big1,
        // .NET BigInteger division truncates toward zero; num-bigint's
        // division floors, so adjust the quotient when the signs differ and
        // the division was inexact.
        "divide" => {
            let q = &big0 / &big1;
            let r = &big0 % &big1;
            if !r.is_zero() && big0.is_negative() != big1.is_negative() {
                q - 1
            } else {
                q
            }
        }
        // .NET's remainder takes the dividend's sign; mod_floor already does.
        "remainder" => big0.mod_floor(&big1),
        "negate" => -&big0,
        "not" => !&big0,
        "and" => &big0 & &big1,
        "or" => &big0 | &big1,
        "xor" => &big0 ^ &big1,
        "shift_left" => &big0 << (unwrap_i32(&args[1]) as usize),
        "shift_right" => &big0 >> (unwrap_i32(&args[1]) as usize),
        "pow" => {
            let mut v = BigInt::from(1);
            for _ in 0..big1.to_u32().unwrap_or(0) {
                v = &v * &big0;
            }
            v
        }
        "is_integer_text" | "to_long" | "fits_long" | "to_double" | "to_string" | "compare" | "equals" | "hash" | "mod_pow" | "gcd" | "bit_length" | "to_hex" => big0.clone(),
        other => panic!("unhandled bigint op {other}"),
    };

    match op {
        "to_long" => outs[0] = Val::I64(big0.to_i64().unwrap_or(0)),
        "fits_long" => outs[0] = Val::I32(big0.to_i64().is_some() as i32),
        "to_double" => outs[0] = Val::F64(big0.to_f64().unwrap_or(f64::NAN).to_bits()),
        "to_string" => outs[0] = new_text(caller, &big0.to_string()),
        "is_integer_text" => {
            let t = text_of(caller, &args[0]);
            let t = t.trim();
            let digits = t.strip_prefix('-').unwrap_or(t.strip_prefix('+').unwrap_or(t));
            outs[0] = Val::I32((!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())) as i32);
        }
        "compare" => {
            outs[0] = Val::I32(match big0.cmp(&big1) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            });
        }
        "equals" => outs[0] = Val::I32((big0 == big1) as i32),
        "hash" => {
            let m = big0.mod_floor(&BigInt::from(2u32).pow(32u32)).to_i32().unwrap_or(0);
            outs[0] = Val::I32(m);
        }
        "bit_length" => {
            outs[0] = Val::I32(if big0.is_zero() {
                0
            } else if big0.is_negative() {
                (!&big0).bits() as i32
            } else {
                big0.bits() as i32
            });
        }
        "mod_pow" => {
            // (base ** exponent) mod modulus, as .NET's ModPow: the
            // exponent non-negative, the result taking the modulus's sign.
            let modulus = big(caller, &args[2]);
            let negative = modulus.is_negative();
            let m = modulus.abs();
            let mut result = BigInt::from(1) % &m;
            let mut base = &big0 % &m;
            let mut rest = big1.abs();

            while !rest.is_zero() {
                if &rest % 2u32 == BigInt::from(1) {
                    result = (&result * &base) % &m;
                }

                base = (&base * &base) % &m;
                rest >>= 1;
            }

            if negative {
                result = -result;
            }

            outs[0] = Val::ExternRef(Some(ExternRef::new(caller, result).expect("externref")));
        }
        "gcd" => {
            let a = big0.abs();
            let b = big1.abs();
            outs[0] = Val::ExternRef(Some(ExternRef::new(caller, a.gcd(&b)).expect("externref")));
        }
        "to_hex" => {
            // Two's complement in as few hex digits as hold the value and
            // its sign, as .NET writes a BigInteger under X.
            let upper = unwrap_i32(&args[1]) != 0;
            let magnitude_hex = if big0.is_negative() {
                (!&big0).to_str_radix(16)
            } else {
                big0.to_str_radix(16)
            };
            let digits = magnitude_hex.len();
            let mut text = if big0.is_negative() {
                let wrap = BigInt::from(1) << (4 * digits);
                format!("{:0>width$}", (&wrap + &big0).to_str_radix(16), width = digits)
            } else {
                big0.to_str_radix(16)
            };

            if big0.is_zero() {
                text = "0".to_string();
            } else if big0.is_positive() {
                if text.starts_with(['8', '9', 'a', 'b', 'c', 'd', 'e', 'f']) {
                    text = format!("0{text}");
                }
            } else if text.starts_with(['0', '1', '2', '3', '4', '5', '6', '7']) {
                text = format!("f{text}");
            }

            let text = if upper { text.to_uppercase() } else { text };
            outs[0] = new_text(caller, &text);
        }
        _ => outs[0] = Val::ExternRef(Some(ExternRef::new(caller, produce).expect("externref"))),
    }

    Ok(())
}

// ---- value helpers ------------------------------------------------------

fn new_text(caller: &mut wasmtime::Caller<'_, HostState>, text: &str) -> Val {
    let e = ExternRef::new(caller, text.to_string()).expect("externref");
    Val::ExternRef(Some(e))
}

fn as_array(caller: &wasmtime::Caller<'_, HostState>, v: &Val) -> Option<wasmtime::Rooted<ArrayRef>> {
    match v {
        Val::AnyRef(Some(a)) => a.as_array(caller).ok().flatten(),
        _ => None,
    }
}

fn text_of(caller: &wasmtime::Caller<'_, HostState>, v: &Val) -> String {
    match v {
        Val::ExternRef(Some(e)) => {
            if let Ok(Some(data)) = e.data(caller) {
                if let Some(s) = data.downcast_ref::<String>() {
                    return s.clone();
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn unwrap_i32(v: &Val) -> i32 {
    v.i32().unwrap_or(0)
}

fn unwrap_i64(v: &Val) -> i64 {
    v.i64().unwrap_or(0)
}

fn unwrap_f64(v: &Val) -> f64 {
    v.f64().unwrap_or(0.0)
}

fn zero_val(t: &ValType) -> Val {
    match t {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::from(0.0f32),
        ValType::F64 => Val::from(0.0f64),
        ValType::Ref(_) => Val::ExternRef(None),
        other => panic!("unsupported result {other:?}"),
    }
}

fn fill_stdin(state: &mut HostState) {
    if state.stdin.is_none() {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut bytes);
        state.stdin = Some(bytes);
    }
}

// .NET-flavoured error codes the loader answers: 1 no such file with its
// directory there either way, 2 not found or not a directory in the path,
// 3 permission, 4 anything else. (The loader's code_for.)
fn error_code(e: &std::io::Error, path: &str) -> i32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            if std::path::Path::new(path).parent().map(|p| p.exists()).unwrap_or(false) {
                1
            } else {
                2
            }
        }
        std::io::ErrorKind::PermissionDenied => 3,
        _ => 4,
    }
}

fn attempt(caller: &mut wasmtime::Caller<'_, HostState>, path: &str, action: impl FnOnce() -> std::io::Result<()>) -> i32 {
    match action() {
        Ok(()) => {
            caller.data_mut().last_error = 0;
            0
        }
        Err(e) => {
            let code = error_code(&e, path);
            caller.data_mut().last_error = code;
            code
        }
    }
}

fn file_op(
    caller: &mut wasmtime::Caller<'_, HostState>,
    name: &str,
    args: &[Val],
    outs: &mut [Val],
    result_types: &[ValType],
) -> Result<(), wasmtime::Error> {
    match name {
        "file_system" => {
            outs[0] = Val::I32(caller.data().real_fs as i32);
            Ok(())
        }
        "file_current_directory" => {
            let cwd = std::env::current_dir().unwrap_or_default();
            outs[0] = new_text(caller, &cwd.to_string_lossy());
            Ok(())
        }
        "file_kind" => {
            let path = text_of(caller, &args[0]);
            let meta = std::fs::metadata(&path);
            outs[0] = Val::I32(match meta {
                Err(e) => {
                    caller.data_mut().last_error = error_code(&e, &path);
                    0
                }
                Ok(m) if m.is_dir() => 2,
                Ok(_) => 1,
            });
            Ok(())
        }
        "file_last_error" => {
            outs[0] = Val::I32(caller.data().last_error);
            Ok(())
        }
        "file_write_text" => {
            let path = text_of(caller, &args[0]);
            let content: String = text_of(caller, &args[1]);
            let append = unwrap_i32(&args[2]) != 0;
            let bytes: Vec<u8> = content.chars().map(|c| c as u32 as u8).collect();
            let code = attempt(caller, &path, || {
                if append {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
                    f.write_all(&bytes)
                } else {
                    std::fs::write(&path, &bytes)
                }
            });
            outs[0] = Val::I32(code);
            Ok(())
        }
        "file_delete" => {
            let path = text_of(caller, &args[0]);
            // A missing file is success, unless its directory is gone too.
            let code = attempt(caller, &path, || std::fs::remove_file(&path));
            outs[0] = Val::I32(if code == 1 { 0 } else { code });
            Ok(())
        }
        "file_rename" => {
            let from = text_of(caller, &args[0]);
            let to = text_of(caller, &args[1]);
            let code = attempt(caller, &from, || std::fs::rename(&from, &to));
            outs[0] = Val::I32(code);
            Ok(())
        }
        "directory_set_current" => {
            let path = text_of(caller, &args[0]);
            let code = attempt(caller, &path, || std::env::set_current_dir(&path));
            outs[0] = Val::I32(code);
            Ok(())
        }
        "directory_create" => {
            let path = text_of(caller, &args[0]);
            let code = attempt(caller, &path, || std::fs::create_dir_all(&path));
            outs[0] = Val::I32(code);
            Ok(())
        }
        "directory_delete" => {
            let path = text_of(caller, &args[0]);
            let recursive = unwrap_i32(&args[1]) != 0;
            let code = attempt(caller, &path, || {
                if recursive {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_dir(&path)
                }
            });
            outs[0] = Val::I32(code);
            Ok(())
        }
        "directory_entries" => {
            let path = text_of(caller, &args[0]);
            let want_dirs = unwrap_i32(&args[1]) != 0;
            match std::fs::read_dir(&path) {
                Err(e) => {
                    let code = error_code(&e, &path);
                    caller.data_mut().last_error = code;
                    outs[0] = Val::I32(-code);
                    Ok(())
                }
                Ok(read) => {
                    let mut names = Vec::new();
                    for entry in read.flatten() {
                        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        if is_dir == want_dirs {
                            names.push(entry.file_name().to_string_lossy().into_owned());
                        }
                    }
                    let count = names.len() as i32;
                    caller.data_mut().entries = names;
                    outs[0] = Val::I32(count);
                    Ok(())
                }
            }
        }
        "directory_entry" => {
            let i = unwrap_i32(&args[0]);
            let name = caller.data().entries.get(i as usize).cloned().unwrap_or_default();
            outs[0] = new_text(caller, &name);
            Ok(())
        }
        other => {
            // Unknown: fail softly, as no_files did.
            for (i, t) in result_types.iter().enumerate() {
                outs[i] = zero_val(t);
            }
            panic!("unhandled host op {other}");
        }
    }
}

fn base64_decode(text: &str) -> Vec<u8> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes: Vec<u8> = text.bytes().filter(|b| *b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in bytes {
        buffer = (buffer << 6) | value(byte).unwrap_or(0);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    out
}

fn rand_u32() -> u32 {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    let mut x = seed | 1;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}
