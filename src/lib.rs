#![allow(non_snake_case)]

mod config;
mod pattern;
mod runtime;
mod trampoline;

use std::ffi::c_void;

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

#[unsafe(no_mangle)]
/// # Safety
///
/// Windows calls this function as the DLL entry point. The caller must pass the
/// module handle and notification reason according to the `DllMain` contract.
pub unsafe extern "system" fn DllMain(
    module: *mut c_void,
    reason: u32,
    _reserved: *mut c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        unsafe {
            runtime::process_attach(module);
        }
    } else if reason == DLL_PROCESS_DETACH {
        unsafe {
            runtime::process_detach();
        }
    }
    1
}
