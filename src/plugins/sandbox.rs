//! Running plugin code: wasmi, an interpreter, with fuel (an instruction
//! budget), a memory cap and no imports but `env.host_log`. Each call gets
//! a fresh instance, so nothing survives between calls.
//!
//! ABI (docs/PLUGINS.md): exports `memory`, `alloc(len) -> ptr`,
//! `dealloc(ptr, len)`, `run_command(ptr, len) -> i64` and optionally
//! `render(ptr, len) -> i64`; the result packs `ptr << 32 | len`.

use wasmi::{
    Caller, Config, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    TrapCode,
};

/// Fuel for a command (run off the UI thread).
pub const COMMAND_FUEL: u64 = 50_000_000;
/// Fuel for a render hook (run while drawing, so smaller; cached).
pub const RENDER_FUEL: u64 = 5_000_000;
/// The most memory a plugin may have.
pub const MAX_MEMORY: usize = 32 << 20;
pub const MAX_INPUT: usize = 256 << 10;
pub const MAX_OUTPUT: usize = 64 << 10;
const MAX_LOG_LINES: usize = 20;
const MAX_LOG_LINE: usize = 500;

/// A checked, compiled plugin binary.
pub struct Compiled {
    engine: Engine,
    module: Module,
}

struct Host {
    limits: StoreLimits,
    log: Vec<String>,
}

/// Compile `wasm`: it must import nothing but `env.host_log`.
pub fn compile(wasm: &[u8]) -> Result<Compiled, String> {
    if wasm.len() as u64 > super::MAX_WASM {
        return Err("plugin.wasm is too big".into());
    }
    let mut config = Config::default();
    config.consume_fuel(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).map_err(|err| format!("invalid wasm: {err}"))?;
    for import in module.imports() {
        if (import.module(), import.name()) != ("env", "host_log") {
            return Err(format!(
                "it imports {}.{}, which NoteSec doesn't provide (only env.host_log)",
                import.module(),
                import.name()
            ));
        }
    }
    Ok(Compiled { engine, module })
}

/// What a call gave back: its output bytes and what it logged.
#[derive(Debug)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub log: Vec<String>,
}

fn describe(err: wasmi::Error) -> String {
    match err.as_trap_code() {
        Some(TrapCode::OutOfFuel) => "it ran too long (out of fuel) and was stopped".into(),
        Some(TrapCode::GrowthOperationLimited) => "it asked for more memory than allowed".into(),
        Some(code) => format!("it crashed ({code})"),
        None => format!("it failed: {err}"),
    }
}

/// Call `export` with `input` in a fresh instance with `fuel`.
pub fn call(c: &Compiled, export: &str, input: &[u8], fuel: u64) -> Result<Output, String> {
    if input.len() > MAX_INPUT {
        return Err("the input is too big for a plugin".into());
    }
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY)
        .memories(1)
        .tables(1)
        .table_elements(10_000)
        .instances(1)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(
        &c.engine,
        Host {
            limits,
            log: Vec::new(),
        },
    );
    store.limiter(|host| &mut host.limits);
    store.set_fuel(fuel).map_err(|err| err.to_string())?;
    let mut linker = <Linker<Host>>::new(&c.engine);
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, Host>, ptr: i32, len: i32| {
                if caller.data().log.len() >= MAX_LOG_LINES {
                    return; // rate limit: the rest is dropped
                }
                let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                    return;
                };
                let (ptr, len) = (ptr as u32 as usize, (len as u32 as usize).min(MAX_LOG_LINE));
                let mut buf = vec![0u8; len];
                if memory.read(&caller, ptr, &mut buf).is_ok() {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    caller.data_mut().log.push(line);
                }
            },
        )
        .map_err(|err| err.to_string())?;
    let instance = linker
        .instantiate_and_start(&mut store, &c.module)
        .map_err(describe)?;
    let memory = instance
        .get_memory(&store, "memory")
        .ok_or("it doesn't export its memory")?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&store, "alloc")
        .map_err(|_| "it doesn't export alloc(len) -> ptr")?;
    let func = instance
        .get_typed_func::<(i32, i32), i64>(&store, export)
        .map_err(|_| format!("it doesn't export {export}(ptr, len) -> i64"))?;
    let len = input.len() as i32;
    let ptr = alloc.call(&mut store, len).map_err(describe)? as u32 as usize;
    memory
        .write(&mut store, ptr, input)
        .map_err(|_| "alloc returned memory it doesn't have")?;
    let packed = func.call(&mut store, (ptr as i32, len)).map_err(describe)? as u64;
    let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
    if out_len > MAX_OUTPUT {
        return Err(format!("its answer is too big ({out_len} bytes)"));
    }
    let mut bytes = vec![0u8; out_len];
    memory
        .read(&store, out_ptr, &mut bytes)
        .map_err(|_| "its answer points outside its memory")?;
    if let Ok(dealloc) = instance.get_typed_func::<(i32, i32), ()>(&store, "dealloc") {
        let _ = dealloc.call(&mut store, (ptr as i32, len));
    }
    let log = std::mem::take(&mut store.data_mut().log);
    Ok(Output { bytes, log })
}
