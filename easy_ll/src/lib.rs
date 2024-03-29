//! A very simple wrapper for LLVM that can JIT functions written as IR strings.

extern crate llvm_sys as llvm;

use std::error::Error;
use std::ffi::{CStr, CString, NulError};
use std::fmt;
use std::result::Result;
use std::ops::Drop;
use std::os::raw::c_char;
use std::sync::{Once, ONCE_INIT};

use llvm::prelude::{LLVMContextRef, LLVMModuleRef};
use llvm::execution_engine::{LLVMExecutionEngineRef, LLVMMCJITCompilerOptions};
use llvm::analysis::LLVMVerifierFailureAction;
use llvm::transforms::pass_manager_builder as pmb;

#[cfg(test)]
mod tests;

// Helper objects to make sure we only initialize once
static ONCE: Once = ONCE_INIT;
static mut initialize_failed: bool = false;

/// Error type returned by easy_ll.
#[derive(Debug)]
pub struct LlvmError(String);

impl LlvmError {
    pub fn new(description: &str) -> LlvmError { LlvmError(description.to_string()) }

    pub fn to_string(self) -> String { self.0 }
}

impl fmt::Display for LlvmError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for LlvmError {
    fn description(&self) -> &str { &self.0 }

    fn cause(&self) -> Option<&Error> { None }
}

impl From<NulError> for LlvmError {
    fn from(_: NulError) -> LlvmError { LlvmError::new("Null byte in string") }
}

/// The type of our "run" function pointer.
type RunFunc = extern "C" fn(i64) -> i64;

/// A compiled module returned by `compile_module`, wrapping a `run` function that takes `i64`
/// and returns `i64`. This structure includes (and manages) an LLVM execution engine, which is
/// freed when this structure is dropped.
#[derive(Debug)]
pub struct CompiledModule {
    context: LLVMContextRef,
    engine: Option<LLVMExecutionEngineRef>,
    function: Option<RunFunc>
}

impl CompiledModule {
    /// Call the module's `run` function.
    pub fn run(&self, arg: i64) -> i64 {
        (self.function.unwrap())(arg)
    }
}

impl Drop for CompiledModule {
    /// Disposes of the LLVM execution engine and compiled function.
    fn drop(&mut self) {
        unsafe {
            self.engine.map(|e| {
                llvm::execution_engine::LLVMDisposeExecutionEngine(e)
            });
            llvm::core::LLVMContextDispose(self.context);
        }
    }
}

/// Compile a string of LLVM IR (in human readable format) into a `CompiledModule` that can then
/// be executed. The LLVM IR should contain an entry point function called `run` that takes `i64`
/// and returns `i64`, which will be called by `CompiledModule::run`.
pub fn compile_module(code: &str) -> Result<CompiledModule, LlvmError> {
    unsafe {
        // Initialize LLVM
        ONCE.call_once(|| initialize());
        if initialize_failed {
            return Err(LlvmError::new("LLVM initialization failed"))
        }

        // Create an LLVM context
        let context = llvm::core::LLVMContextCreate();
        if context.is_null() {
            return Err(LlvmError::new("LLVMContextCreate returned null"))
        }

        // Create a CompiledModule to wrap the context and our result (will clean it on Drop).
        let mut result = CompiledModule { context: context, engine: None, function: None };

        // Parse the IR to get an LLVMModuleRef
        let module = try!(parse_module(context, code));

        // Validate and optimize the module
        try!(verify_module(module));
        try!(check_run_function(module));
        try!(optimize_module(module));

        // Create an execution engine for the module and find its run function
        let engine = try!(create_exec_engine(module));
        result.engine = Some(engine);
        result.function = Some(try!(find_run_function(engine)));

        Ok(result)
    }
}

/// Initialize LLVM or save an error message in `initialize_failed` if this does not work.
/// We call this function only once in cases some steps are expensive.
fn initialize() {
    unsafe {
        if llvm::target::LLVM_InitializeNativeTarget() != 0 {
            initialize_failed = true;
            return;
        }
        if llvm::target::LLVM_InitializeNativeAsmPrinter() != 0 {
            initialize_failed = true;
            return;
        }
        if llvm::target::LLVM_InitializeNativeAsmParser() != 0 {
            initialize_failed = true;
            return;
        }
        llvm::execution_engine::LLVMLinkInMCJIT();
    }
}

/// Parse a string of IR code into an `LLVMModuleRef` for the given context.
unsafe fn parse_module(context: LLVMContextRef, code: &str) -> Result<LLVMModuleRef, LlvmError> {
    // Create an LLVM memory buffer around the code
    let code_len = code.len();
    let name = try!(CString::new("module"));
    let code = try!(CString::new(code));
    let buffer = llvm::core::LLVMCreateMemoryBufferWithMemoryRange(
        code.as_ptr(), code_len, name.as_ptr(), 0);
    if buffer.is_null() {
        return Err(LlvmError::new("LLVMCreateMemoryBufferWithMemoryRange failed"))
    }

    // Parse IR into a module
    let mut module = 0 as LLVMModuleRef;
    let mut error_str = 0 as *mut c_char;
    let result_code = llvm::ir_reader::LLVMParseIRInContext(
        context, buffer, &mut module, &mut error_str);
    if result_code != 0 {
        let msg = format!("Compile error: {}", CStr::from_ptr(error_str).to_str().unwrap());
        return Err(LlvmError(msg));
    }

    Ok(module)
}

/// Verify a module using LLVM's verifier (for basic block structure, etc).
unsafe fn verify_module(module: LLVMModuleRef) -> Result<(), LlvmError> {
    let mut error_str = 0 as *mut c_char;
    let result_code = llvm::analysis::LLVMVerifyModule(
        module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut error_str);
    if result_code != 0 {
        let msg = format!("Module verification failed: {}",
            CStr::from_ptr(error_str).to_str().unwrap());
        return Err(LlvmError(msg));
    }
    Ok(())
}

/// Check that a module has a "run" function of type i64 -> i64.
unsafe fn check_run_function(module: LLVMModuleRef) -> Result<(), LlvmError> {
    let run = CString::new("run").unwrap();
    let func = llvm::core::LLVMGetNamedFunction(module, run.as_ptr());
    if func.is_null() {
        println!("EEEK");
        return Err(LlvmError::new("No run function in module"));
    }
    let c_str = llvm::core::LLVMPrintTypeToString(llvm::core::LLVMTypeOf(func));
    let func_type = CStr::from_ptr(c_str).to_str().unwrap();
    if func_type != "i64 (i64)*" {
        return Err(LlvmError(format!("Run function has wrong type: {}", func_type)));
    }
    Ok(())
}

/// Optimize an LLVM module using our chosen passes (currently uses standard passes for -O2).
unsafe fn optimize_module(module: LLVMModuleRef) -> Result<(), LlvmError> {
    let manager = llvm::core::LLVMCreatePassManager();
    if manager.is_null() {
        return Err(LlvmError::new("LLVMCreatePassManager returned null"))
    }
    let builder = pmb::LLVMPassManagerBuilderCreate();
    if builder.is_null() {
        return Err(LlvmError::new("LLVMPassManagerBuilderCreate returned null"))
    }
    // TODO: not clear we need both Module and LTO calls here; just LTO might work
    pmb::LLVMPassManagerBuilderSetOptLevel(builder, 2);
    pmb::LLVMPassManagerBuilderPopulateModulePassManager(builder, manager);
    pmb::LLVMPassManagerBuilderPopulateLTOPassManager(builder, manager, 1, 1);
    pmb::LLVMPassManagerBuilderDispose(builder);
    llvm::core::LLVMRunPassManager(manager, module);
    llvm::core::LLVMDisposePassManager(manager);
    Ok(())
}

/// Create an MCJIT execution engine for a given module.
unsafe fn create_exec_engine(module: LLVMModuleRef) -> Result<LLVMExecutionEngineRef, LlvmError> {
    let mut engine = 0 as LLVMExecutionEngineRef;
    let mut error_str = 0 as *mut c_char;
    let mut options: LLVMMCJITCompilerOptions = std::mem::uninitialized();
    let options_size = std::mem::size_of::<LLVMMCJITCompilerOptions>();
    llvm::execution_engine::LLVMInitializeMCJITCompilerOptions(&mut options, options_size);
    options.OptLevel = 2;
    let result_code = llvm::execution_engine::LLVMCreateMCJITCompilerForModule(
        &mut engine, module, &mut options, options_size, &mut error_str);
    if result_code != 0 {
        let msg = format!("Creating execution engine failed: {}",
            CStr::from_ptr(error_str).to_str().unwrap());
        return Err(LlvmError(msg));
    }
    Ok(engine)
}

/// Get a pointer to the "run" function in an execution engine.
unsafe fn find_run_function(engine: LLVMExecutionEngineRef) -> Result<RunFunc, LlvmError> {
    let run = CString::new("run").unwrap();
    let func_addr = llvm::execution_engine::LLVMGetFunctionAddress(engine, run.as_ptr());
    if func_addr == 0 {
        return Err(LlvmError::new("No run function in module"))
    }
    let function: RunFunc = std::mem::transmute(func_addr);
    Ok(function)
}
