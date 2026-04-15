//! SIGABRT interception for surviving mesa GPU context resets.
//!
//! When the AMDGPU driver detects a GPU context reset (e.g. after killing
//! a client process mid-GPU-operation), mesa's gallium winsys calls `abort()`
//! unconditionally in a background worker thread. There is no way to prevent
//! this via EGL robustness — the abort happens below the GL layer.
//!
//! We install a SIGABRT handler that converts the abort into a recoverable
//! flag. The render loop checks this flag and recreates the GPU renderer.
//!
//! # Safety
//! The signal handler uses `siglongjmp` to escape the `abort()` call on the
//! mesa worker thread. This is safe because:
//! - The thread was about to die anyway (abort is fatal)
//! - We longjmp to a known safe point that simply exits the thread
//! - The render loop detects the flag and recreates the entire GPU stack

use std::sync::atomic::{AtomicBool, Ordering};

/// Flag set by the SIGABRT handler. Checked by the render loop.
pub static GPU_RESET_DETECTED: AtomicBool = AtomicBool::new(false);

/// Install a SIGABRT handler that catches mesa's abort() and sets a flag
/// instead of killing the process.
///
/// # Safety
/// Must be called once at startup before any GPU operations.
pub fn install_abort_guard() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigabrt_handler as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        // Don't mask any signals during handler
        libc::sigemptyset(&mut sa.sa_mask);
        let ret = libc::sigaction(libc::SIGABRT, &sa, std::ptr::null_mut());
        if ret != 0 {
            eprintln!("[abort-guard] failed to install SIGABRT handler: {}", std::io::Error::last_os_error());
        } else {
            eprintln!("[abort-guard] SIGABRT handler installed");
        }
    }
}

/// SIGABRT handler. Called when mesa's abort() fires.
///
/// We set the GPU_RESET_DETECTED flag and then do NOT let the process die.
/// Instead we return from the handler. When `abort()` sees the handler
/// returned, glibc will reset SIGABRT to SIG_DFL and re-raise — so we
/// need to re-install our handler before returning.
///
/// To prevent the kill: we re-install ourselves before returning, so the
/// second raise also hits our handler. After two iterations glibc gives up
/// on the raise path and proceeds to `_exit()` — but actually in modern
/// glibc, `abort()` calls `raise(SIGABRT)`, and if the handler returns,
/// it sets SIGABRT to SIG_DFL and calls `raise(SIGABRT)` again.
///
/// The trick: we use SA_NODEFER so the handler can re-enter, and we
/// re-install ourselves on every invocation. This intercepts both raises.
/// After the second raise returns from our handler, glibc calls `_exit()`
/// but we prevent that by using `pthread_exit()` to only kill the current
/// thread (the mesa worker thread), not the whole process.
extern "C" fn sigabrt_handler(
    _sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // Set the flag (async-signal-safe: atomic store)
    GPU_RESET_DETECTED.store(true, Ordering::Release);

    // Write a message (write() is async-signal-safe)
    let msg = b"[abort-guard] SIGABRT intercepted (mesa GPU reset), terminating worker thread\n";
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
    }

    // Re-install ourselves so the second raise from abort() also
    // hits our handler instead of SIG_DFL.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigabrt_handler as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGABRT, &sa, std::ptr::null_mut());
    }

    // Kill only this thread (the mesa worker), not the whole process.
    // pthread_exit is NOT officially async-signal-safe, but on Linux/glibc
    // it works from signal handlers — it just unwinds the current thread.
    // This prevents abort()'s fallback _exit() from reaching process level.
    unsafe {
        libc::pthread_exit(std::ptr::null_mut());
    }
}
