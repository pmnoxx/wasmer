#![deny(
    nonstandard_style,
    unused_imports,
    unused_mut,
    unused_variables,
    unused_unsafe,
    unreachable_patterns
)]
#![cfg_attr(
    all(not(target_os = "windows"), not(target_arch = "aarch64")),
    deny(dead_code)
)]
#![cfg_attr(nightly, feature(unwind_attributes))]
#![doc(html_favicon_url = "https://wasmer.io/static/icons/favicon.ico")]
#![doc(html_logo_url = "https://github.com/wasmerio.png?size=200")]

mod abi;
mod compiler;
mod config;
mod object_file;
mod trampoline;
mod translator;

pub use crate::compiler::LLVMCompiler;
pub use crate::config::{CompiledKind, InkwellMemoryBuffer, InkwellModule, LLVMCallbacks, LLVM};
