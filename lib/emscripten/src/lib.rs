#[macro_use]
extern crate wasmer_runtime_core;

use std::cell::UnsafeCell;
use std::{f64, ffi::c_void};
use wasmer_runtime_core::{
    error::CallResult,
    export::Export,
    func,
    global::Global,
    import::ImportObject,
    imports,
    memory::Memory,
    table::Table,
    types::{ElementType, MemoryDescriptor, TableDescriptor, Value},
    units::Pages,
    vm::Ctx,
    Func, Instance, Module,
};

#[macro_use]
mod macros;
//#[cfg(test)]
mod file_descriptor;
pub mod stdio;

// EMSCRIPTEN APIS
mod env;
mod errno;
mod exception;
mod io;
mod jmp;
mod linking;
mod lock;
mod math;
mod memory;
mod nullfunc;
mod process;
mod signal;
mod storage;
mod syscalls;
mod time;
mod utils;
mod varargs;

pub use self::storage::{align_memory, static_alloc};
pub use self::utils::{
    allocate_cstr_on_stack, allocate_on_stack, get_emscripten_memory_size,
    get_emscripten_table_size, is_emscripten_module,
};

// TODO: Magic number - how is this calculated?
const TOTAL_STACK: u32 = 5_242_880;
// TODO: Magic number - how is this calculated?
const DYNAMICTOP_PTR_DIFF: u32 = 1088;
// TODO: make this variable
const STATIC_BUMP: u32 = 215_536;

// The address globals begin at. Very low in memory, for code size and optimization opportunities.
// Above 0 is static memory, starting with globals.
// Then the stack.
// Then 'dynamic' memory for sbrk.
const GLOBAL_BASE: u32 = 1024;
const STATIC_BASE: u32 = GLOBAL_BASE;

fn stacktop(static_bump: u32) -> u32 {
    align_memory(dynamictop_ptr(static_bump) + 4)
}

fn stack_max(static_bump: u32) -> u32 {
    stacktop(static_bump) + TOTAL_STACK
}

fn dynamic_base(static_bump: u32) -> u32 {
    align_memory(stack_max(static_bump))
}

fn dynamictop_ptr(static_bump: u32) -> u32 {
    static_bump + DYNAMICTOP_PTR_DIFF
}

pub struct EmscriptenData<'a> {
    pub malloc: Func<'a, u32, u32>,
    pub free: Func<'a, u32>,
    pub memalign: Option<Func<'a, (u32, u32), u32>>,
    pub memset: Func<'a, (u32, u32, u32), u32>,
    pub stack_alloc: Func<'a, u32, u32>,

    pub jumps: Vec<UnsafeCell<[u32; 27]>>,
}

impl<'a> EmscriptenData<'a> {
    pub fn new(instance: &'a mut Instance) -> EmscriptenData<'a> {
        let malloc = instance.func("_malloc").unwrap();
        let free = instance.func("_free").unwrap();
        let memalign = if let Ok(func) = instance.func("_memalign") {
            Some(func)
        } else {
            None
        };
        let memset = instance.func("_memset").unwrap();
        let stack_alloc = instance.func("stackAlloc").unwrap();

        EmscriptenData {
            malloc,
            free,
            memalign,
            memset,
            stack_alloc,
            jumps: Vec::new(),
        }
    }
}

pub fn run_emscripten_instance(
    _module: &Module,
    instance: &mut Instance,
    path: &str,
    args: Vec<&str>,
) -> CallResult<()> {
    let mut data = EmscriptenData::new(instance);
    let data_ptr = &mut data as *mut _ as *mut c_void;
    instance.context_mut().data = data_ptr;

    if let Ok(_func) = instance.dyn_func("___emscripten_environ_constructor") {
        instance.call("___emscripten_environ_constructor", &[])?;
    }

    // println!("running emscripten instance");

    let main_func = instance.dyn_func("_main")?;
    let num_params = main_func.signature().params().len();
    let _result = match num_params {
        2 => {
            let (argc, argv) = store_module_arguments(instance.context_mut(), path, args);
            instance.call("_main", &[Value::I32(argc as i32), Value::I32(argv as i32)])?;
        }
        0 => {
            instance.call("_main", &[])?;
        }
        _ => panic!(
            "The emscripten main function has received an incorrect number of params {}",
            num_params
        ),
    };

    // TODO atinit and atexit for emscripten
    // println!("{:?}", data);
    Ok(())
}

fn store_module_arguments(ctx: &mut Ctx, path: &str, args: Vec<&str>) -> (u32, u32) {
    let argc = args.len() + 1;

    let mut args_slice = vec![0; argc];
    args_slice[0] = unsafe { allocate_cstr_on_stack(ctx, path).0 };
    for (slot, arg) in args_slice[1..argc].iter_mut().zip(args.iter()) {
        *slot = unsafe { allocate_cstr_on_stack(ctx, &arg).0 };
    }

    let (argv_offset, argv_slice): (_, &mut [u32]) =
        unsafe { allocate_on_stack(ctx, ((argc + 1) * 4) as u32) };
    assert!(!argv_slice.is_empty());
    for (slot, arg) in argv_slice[0..argc].iter_mut().zip(args_slice.iter()) {
        *slot = *arg
    }
    argv_slice[argc] = 0;

    (argc as u32, argv_offset)
}

pub fn emscripten_set_up_memory(memory: &Memory, globals: &EmscriptenGlobalsData) {
    let dynamictop_ptr = globals.dynamictop_ptr;
    let stack_max = globals.stack_max;

    let dynamic_base = align_memory(stack_max);

    memory.view::<u32>()[(dynamictop_ptr / 4) as usize].set(dynamic_base);
}

pub struct EmscriptenGlobalsData {
    abort: u64,
    // Env namespace
    stacktop: u32,
    stack_max: u32,
    dynamictop_ptr: u32,
    memory_base: u32,
    table_base: u32,
    temp_double_ptr: u32,

    // Global namespace
    infinity: f64,
    nan: f64,
}

pub struct EmscriptenGlobals {
    // The emscripten data
    pub data: EmscriptenGlobalsData,
    // The emscripten memory
    pub memory: Memory,
    pub table: Table,
    pub memory_min: Pages,
    pub memory_max: Option<Pages>,
}

impl EmscriptenGlobals {
    pub fn new(module: &Module /*, static_bump: u32 */) -> Self {
        let (table_min, table_max) = get_emscripten_table_size(&module);
        let (memory_min, memory_max) = get_emscripten_memory_size(&module);

        // Memory initialization
        let memory_type = MemoryDescriptor {
            minimum: memory_min,
            maximum: memory_max,
            shared: false,
        };
        let memory = Memory::new(memory_type).unwrap();

        let table_type = TableDescriptor {
            element: ElementType::Anyfunc,
            minimum: table_min,
            maximum: table_max,
        };
        let mut table = Table::new(table_type).unwrap();

        let data = {
            let static_bump = STATIC_BUMP;

            let mut STATIC_TOP = STATIC_BASE + static_bump;

            let memory_base = STATIC_BASE;
            let table_base = 0;

            let temp_double_ptr = STATIC_TOP;
            STATIC_TOP += 16;

            let dynamictop_ptr = static_alloc(&mut STATIC_TOP, 4);

            let stacktop = align_memory(STATIC_TOP);
            let stack_max = stacktop + TOTAL_STACK;

            EmscriptenGlobalsData {
                abort: 0,
                stacktop,
                stack_max,
                dynamictop_ptr,
                memory_base,
                table_base,
                temp_double_ptr,

                infinity: std::f64::INFINITY,
                nan: std::f64::NAN,
            }
        };

        emscripten_set_up_memory(&memory, &data);

        Self {
            data,
            memory,
            table,
            memory_min,
            memory_max,
        }
    }
}

pub fn generate_emscripten_env(globals: &mut EmscriptenGlobals) -> ImportObject {
    imports! {
        "env" => {
            "memory" => Export::Memory(globals.memory.clone()),
            "table" => Export::Table(globals.table.clone()),

            // Globals
            "STACKTOP" => Global::new(Value::I32(globals.data.stacktop as i32)),
            "STACK_MAX" => Global::new(Value::I32(globals.data.stack_max as i32)),
            "DYNAMICTOP_PTR" => Global::new(Value::I32(globals.data.dynamictop_ptr as i32)),
            "tableBase" => Global::new(Value::I32(globals.data.table_base as i32)),
            "__table_base" => Global::new(Value::I32(globals.data.table_base as i32)),
            "ABORT" => Global::new(Value::I32(globals.data.abort as i32)),
            "memoryBase" => Global::new(Value::I32(globals.data.memory_base as i32)),
            "__memory_base" => Global::new(Value::I32(globals.data.memory_base as i32)),
            "tempDoublePtr" => Global::new(Value::I32(globals.data.temp_double_ptr as i32)),

            // IO
            "printf" => func!(crate::io::printf),
            "putchar" => func!(crate::io::putchar),
            "___lock" => func!(crate::lock::___lock),
            "___unlock" => func!(crate::lock::___unlock),
            "___wait" => func!(crate::lock::___wait),

            // Env
            "___assert_fail" => func!(crate::env::___assert_fail),
            "_getenv" => func!(crate::env::_getenv),
            "_setenv" => func!(crate::env::_setenv),
            "_putenv" => func!(crate::env::_putenv),
            "_unsetenv" => func!(crate::env::_unsetenv),
            "_getpwnam" => func!(crate::env::_getpwnam),
            "_getgrnam" => func!(crate::env::_getgrnam),
            "___buildEnvironment" => func!(crate::env::___build_environment),
            "___setErrNo" => func!(crate::errno::___seterrno),
            "_getpagesize" => func!(crate::env::_getpagesize),
            "_sysconf" => func!(crate::env::_sysconf),
            "_getaddrinfo" => func!(crate::env::_getaddrinfo),

            // Null func
            "nullFunc_i" => func!(crate::nullfunc::nullfunc_i),
            "nullFunc_ii" => func!(crate::nullfunc::nullfunc_ii),
            "nullFunc_iii" => func!(crate::nullfunc::nullfunc_iii),
            "nullFunc_iiii" => func!(crate::nullfunc::nullfunc_iiii),
            "nullFunc_iiiii" => func!(crate::nullfunc::nullfunc_iiiii),
            "nullFunc_iiiiii" => func!(crate::nullfunc::nullfunc_iiiiii),
            "nullFunc_v" => func!(crate::nullfunc::nullfunc_v),
            "nullFunc_vi" => func!(crate::nullfunc::nullfunc_vi),
            "nullFunc_vii" => func!(crate::nullfunc::nullfunc_vii),
            "nullFunc_viii" => func!(crate::nullfunc::nullfunc_viii),
            "nullFunc_viiii" => func!(crate::nullfunc::nullfunc_viiii),
            "nullFunc_viiiii" => func!(crate::nullfunc::nullfunc_viiiii),
            "nullFunc_viiiiii" => func!(crate::nullfunc::nullfunc_viiiiii),

            // Syscalls
            "___syscall1" => func!(crate::syscalls::___syscall1),
            "___syscall3" => func!(crate::syscalls::___syscall3),
            "___syscall4" => func!(crate::syscalls::___syscall4),
            "___syscall5" => func!(crate::syscalls::___syscall5),
            "___syscall6" => func!(crate::syscalls::___syscall6),
            "___syscall10" => func!(crate::syscalls::___syscall10),
            "___syscall12" => func!(crate::syscalls::___syscall12),
            "___syscall15" => func!(crate::syscalls::___syscall15),
            "___syscall20" => func!(crate::syscalls::___syscall20),
            "___syscall39" => func!(crate::syscalls::___syscall39),
            "___syscall38" => func!(crate::syscalls::___syscall38),
            "___syscall40" => func!(crate::syscalls::___syscall40),
            "___syscall54" => func!(crate::syscalls::___syscall54),
            "___syscall57" => func!(crate::syscalls::___syscall57),
            "___syscall60" => func!(crate::syscalls::___syscall60),
            "___syscall63" => func!(crate::syscalls::___syscall63),
            "___syscall64" => func!(crate::syscalls::___syscall64),
            "___syscall66" => func!(crate::syscalls::___syscall66),
            "___syscall75" => func!(crate::syscalls::___syscall75),
            "___syscall85" => func!(crate::syscalls::___syscall85),
            "___syscall91" => func!(crate::syscalls::___syscall191),
            "___syscall97" => func!(crate::syscalls::___syscall97),
            "___syscall102" => func!(crate::syscalls::___syscall102),
            "___syscall110" => func!(crate::syscalls::___syscall110),
            "___syscall114" => func!(crate::syscalls::___syscall114),
            "___syscall122" => func!(crate::syscalls::___syscall122),
            "___syscall140" => func!(crate::syscalls::___syscall140),
            "___syscall142" => func!(crate::syscalls::___syscall142),
            "___syscall145" => func!(crate::syscalls::___syscall145),
            "___syscall146" => func!(crate::syscalls::___syscall146),
            "___syscall168" => func!(crate::syscalls::___syscall168),
            "___syscall180" => func!(crate::syscalls::___syscall180),
            "___syscall181" => func!(crate::syscalls::___syscall181),
            "___syscall191" => func!(crate::syscalls::___syscall191),
            "___syscall192" => func!(crate::syscalls::___syscall192),
            "___syscall194" => func!(crate::syscalls::___syscall194),
            "___syscall195" => func!(crate::syscalls::___syscall195),
            "___syscall196" => func!(crate::syscalls::___syscall196),
            "___syscall197" => func!(crate::syscalls::___syscall197),
            "___syscall199" => func!(crate::syscalls::___syscall199),
            "___syscall201" => func!(crate::syscalls::___syscall201),
            "___syscall202" => func!(crate::syscalls::___syscall202),
            "___syscall212" => func!(crate::syscalls::___syscall212),
            "___syscall220" => func!(crate::syscalls::___syscall220),
            "___syscall221" => func!(crate::syscalls::___syscall221),
            "___syscall268" => func!(crate::syscalls::___syscall268),
            "___syscall272" => func!(crate::syscalls::___syscall272),
            "___syscall295" => func!(crate::syscalls::___syscall295),
            "___syscall300" => func!(crate::syscalls::___syscall300),
            "___syscall330" => func!(crate::syscalls::___syscall330),
            "___syscall334" => func!(crate::syscalls::___syscall334),
            "___syscall340" => func!(crate::syscalls::___syscall340),

            // Process
            "abort" => func!(crate::process::em_abort),
            "_abort" => func!(crate::process::_abort),
            "abortStackOverflow" => func!(crate::process::abort_stack_overflow),
            "_llvm_trap" => func!(crate::process::_llvm_trap),
            "_fork" => func!(crate::process::_fork),
            "_exit" => func!(crate::process::_exit),
            "_system" => func!(crate::process::_system),
            "_popen" => func!(crate::process::_popen),
            "_endgrent" => func!(crate::process::_endgrent),
            "_execve" => func!(crate::process::_execve),
            "_kill" => func!(crate::process::_kill),
            "_llvm_stackrestore" => func!(crate::process::_llvm_stackrestore),
            "_llvm_stacksave" => func!(crate::process::_llvm_stacksave),
            "_raise" => func!(crate::process::_raise),
            "_sem_init" => func!(crate::process::_sem_init),
            "_sem_post" => func!(crate::process::_sem_post),
            "_sem_wait" => func!(crate::process::_sem_wait),
            "_getgrent" => func!(crate::process::_getgrent),
            "_sched_yield" => func!(crate::process::_sched_yield),
            "_setgrent" => func!(crate::process::_setgrent),
            "_setgroups" => func!(crate::process::_setgroups),
            "_setitimer" => func!(crate::process::_setitimer),
            "_usleep" => func!(crate::process::_usleep),
            "_utimes" => func!(crate::process::_utimes),
            "_waitpid" => func!(crate::process::_waitpid),


            // Signal
            "_sigemptyset" => func!(crate::signal::_sigemptyset),
            "_sigaddset" => func!(crate::signal::_sigaddset),
            "_sigprocmask" => func!(crate::signal::_sigprocmask),
            "_sigaction" => func!(crate::signal::_sigaction),
            "_signal" => func!(crate::signal::_signal),
            "_sigsuspend" => func!(crate::signal::_sigsuspend),

            // Memory
            "abortOnCannotGrowMemory" => func!(crate::memory::abort_on_cannot_grow_memory),
            "_emscripten_memcpy_big" => func!(crate::memory::_emscripten_memcpy_big),
            "_emscripten_get_heap_size" => func!(crate::memory::_emscripten_get_heap_size),
            "_emscripten_resize_heap" => func!(crate::memory::_emscripten_resize_heap),
            "enlargeMemory" => func!(crate::memory::enlarge_memory),
            "getTotalMemory" => func!(crate::memory::get_total_memory),
            "___map_file" => func!(crate::memory::___map_file),

            // Exception
            "___cxa_allocate_exception" => func!(crate::exception::___cxa_allocate_exception),
            "___cxa_throw" => func!(crate::exception::___cxa_throw),

            // Time
            "_gettimeofday" => func!(crate::time::_gettimeofday),
            "_clock_gettime" => func!(crate::time::_clock_gettime),
            "___clock_gettime" => func!(crate::time::_clock_gettime),
            "_clock" => func!(crate::time::_clock),
            "_difftime" => func!(crate::time::_difftime),
            "_asctime" => func!(crate::time::_asctime),
            "_asctime_r" => func!(crate::time::_asctime_r),
            "_localtime" => func!(crate::time::_localtime),
            "_time" => func!(crate::time::_time),
            "_strftime" => func!(crate::time::_strftime),
            "_localtime_r" => func!(crate::time::_localtime_r),
            "_gmtime_r" => func!(crate::time::_gmtime_r),
            "_mktime" => func!(crate::time::_mktime),
            "_gmtime" => func!(crate::time::_gmtime),

            // Math
            "f64-rem" => func!(crate::math::f64_rem),
            "_llvm_log10_f64" => func!(crate::math::_llvm_log10_f64),
            "_llvm_log2_f64" => func!(crate::math::_llvm_log2_f64),
            "_llvm_log10_f32" => func!(crate::math::_llvm_log10_f32),
            "_llvm_log2_f32" => func!(crate::math::_llvm_log2_f64),
            "_emscripten_random" => func!(crate::math::_emscripten_random),

            // Jump
            "__setjmp" => func!(crate::jmp::__setjmp),
            "__longjmp" => func!(crate::jmp::__longjmp),

            // Linking
            "_dlclose" => func!(crate::linking::_dlclose),
            "_dlerror" => func!(crate::linking::_dlerror),
            "_dlopen" => func!(crate::linking::_dlopen),
            "_dlsym" => func!(crate::linking::_dlsym),

        },
        "global" => {
          "NaN" => Global::new(Value::F64(f64::NAN)),
          "Infinity" => Global::new(Value::F64(f64::INFINITY)),
        },
        "global.Math" => {
            "pow" => func!(crate::math::pow),
        },
        "asm2wasm" => {
            "f64-rem" => func!(crate::math::f64_rem),
        },
    }
}

/// The current version of this crate
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
