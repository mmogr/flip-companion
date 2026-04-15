//! ELF symbol interposition to survive mesa GPU context resets.
//!
//! When the AMDGPU driver detects a GPU context reset (e.g. after killing
//! a client process mid-GPU-operation), mesa's gallium winsys calls `abort()`
//! unconditionally from a background worker thread (`util_queue_thread_func`).
//! There is no way to prevent this via EGL robustness — the abort happens
//! below the GL layer in `amdgpu_ctx_set_sw_reset_status`.
//!
//! A SIGABRT signal handler does NOT work here because mesa worker threads
//! block all signals via `sigfillset` + `pthread_sigmask`. glibc's `abort()`
//! calls `raise(SIGABRT)` which stays pending (blocked), our handler never
//! fires, then abort resets to SIG_DFL, unblocks, and kills the process.
//!
//! Instead we use ELF symbol interposition: we define our own `abort()`
//! symbol which the dynamic linker resolves for all shared libraries (mesa
//! calls `abort@plt`). On non-main threads (mesa workers) we set a flag,
//! send SIGUSR1 to the render thread, and park the worker thread forever.
//!
//! The SIGUSR1 handler uses `siglongjmp` to escape whatever blocking call
//! the render thread is stuck in (futex waits on mesa internal mutexes,
//! DRM ioctls, eglWaitSync, etc.). A simple EINTR-based approach doesn't
//! work because mesa's `util_queue_fence_wait` retries after EINTR.
//!
//! On the main thread we forward to glibc's real `abort()` to preserve
//! correct panic/assertion behavior.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

// ── sigsetjmp / siglongjmp FFI ──────────────────────────────────────────
// glibc's sigsetjmp is a macro wrapping __sigsetjmp. The libc crate
// doesn't expose sigjmp_buf, so we define an opaque buffer matching the
// glibc/x86_64 layout (200 bytes, 8-byte aligned).

#[repr(C, align(8))]
pub struct SigJmpBuf {
    _buf: [u8; 200],
}

extern "C" {
    fn __sigsetjmp(env: *mut SigJmpBuf, savesigs: libc::c_int) -> libc::c_int;
    fn siglongjmp(env: *mut SigJmpBuf, val: libc::c_int) -> !;
}

/// Jump buffer for signal recovery inside `render_frame()`.
/// Only accessed by the render thread (sigsetjmp) and signal handler
/// (siglongjmp), which are mutually exclusive on the same thread.
static mut RENDER_JMP_BUF: SigJmpBuf = SigJmpBuf { _buf: [0u8; 200] };

/// Whether RENDER_JMP_BUF contains a valid sigsetjmp checkpoint.
/// Set true by `render_frame()` after sigsetjmp, cleared on return.
/// The SIGUSR1 handler only longjmps when this AND GPU_RESET_DETECTED
/// are both true.
pub static RENDER_JMP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Flag set when our `abort()` override intercepts a mesa GPU reset.
/// Checked by the render loop in drm_platform.rs.
pub static GPU_RESET_DETECTED: AtomicBool = AtomicBool::new(false);

/// Cached pointer to glibc's real `abort()` resolved via `dlsym(RTLD_NEXT)`.
static REAL_ABORT: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// PID of the main thread (main thread has tid == pid on Linux).
static MAIN_PID: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Call `sigsetjmp` on the render thread's recovery buffer.
/// Returns 0 on initial call, non-zero when returning via `siglongjmp`.
///
/// # Safety
/// Must only be called from the render thread.
pub unsafe fn render_sigsetjmp() -> libc::c_int {
    __sigsetjmp(&raw mut RENDER_JMP_BUF, 1)
}

/// Initialize the abort guard. Must be called once at startup before any
/// GPU operations. Resolves and caches glibc's real `abort()` pointer.
/// Installs a SIGUSR1 handler that uses siglongjmp to escape blocked
/// GL calls when a GPU reset is detected.
pub fn install_abort_guard() {
    unsafe {
        // Cache the main thread's pid for thread detection
        let pid = libc::getpid();
        MAIN_PID.store(pid as *mut libc::c_void, Ordering::Release);

        // Resolve glibc's real abort() via RTLD_NEXT so we can forward
        // main-thread aborts (Rust panics, assertions) to the real implementation.
        let sym = libc::dlsym(libc::RTLD_NEXT, b"abort\0".as_ptr() as *const libc::c_char);
        if sym.is_null() {
            let msg = b"[abort-guard] WARNING: dlsym(RTLD_NEXT, \"abort\") returned NULL\n";
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        } else {
            REAL_ABORT.store(sym, Ordering::Release);
        }

        // Install SIGUSR1 handler that uses siglongjmp to escape blocked
        // GL calls. SA_SIGINFO is required for the 3-arg handler signature.
        // We do NOT set SA_RESTART — we want syscalls to return EINTR as
        // a fallback if the longjmp path isn't active.
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigusr1_handler as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
    }
    eprintln!("[abort-guard] abort() symbol interposition active (siglongjmp recovery)");
}

/// SIGUSR1 handler: if a GPU reset was detected and the render thread has
/// an active sigsetjmp checkpoint, longjmp out of the blocked call.
/// Otherwise just return (the signal delivery still interrupts syscalls
/// with EINTR as a fallback).
extern "C" fn sigusr1_handler(
    _sig: libc::c_int,
    _info: *mut libc::siginfo_t,
    _ctx: *mut libc::c_void,
) {
    // Both checks use Acquire to synchronize with the Release stores
    // in abort() (GPU_RESET_DETECTED) and render_frame (RENDER_JMP_ACTIVE).
    if GPU_RESET_DETECTED.load(Ordering::Acquire)
        && RENDER_JMP_ACTIVE.load(Ordering::Acquire)
    {
        unsafe {
            let msg = b"[abort-guard] SIGUSR1: longjmp to render recovery point\n";
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            siglongjmp(&raw mut RENDER_JMP_BUF, 1);
        }
    }
    // If conditions aren't met, just return — the EINTR from signal
    // delivery may still help unblock some syscalls.
}

/// Our `abort()` override. The dynamic linker resolves mesa's `abort@plt`
/// to this function because the main executable's symbols take priority.
///
/// - On non-main threads (mesa workers): set GPU_RESET_DETECTED flag,
///   send SIGUSR1 to the render thread, and park forever.
/// - On the main thread: forward to glibc's real `abort()`.
///
/// `#[no_mangle]` exports it as the plain C symbol `abort`.
/// The `#[used]` static below prevents LTO from stripping this function.
#[no_mangle]
pub extern "C" fn abort() -> ! {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        let main_pid = MAIN_PID.load(Ordering::Acquire) as libc::pid_t;

        if main_pid != 0 && tid != main_pid {
            // Non-main thread — this is a mesa worker calling abort() after
            // detecting a GPU context reset. Intercept it.
            GPU_RESET_DETECTED.store(true, Ordering::Release);

            let msg = b"[abort-guard] abort() intercepted on mesa worker thread, sending SIGUSR1 to render thread\n";
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());

            // Send SIGUSR1 to the main/render thread. If RENDER_JMP_ACTIVE
            // is set, the handler will siglongjmp out of the blocked GL call.
            // If not, the signal still interrupts with EINTR as a fallback.
            libc::syscall(libc::SYS_tgkill, main_pid, main_pid, libc::SIGUSR1);

            // Park this worker thread forever. We can't call pthread_exit()
            // because the worker may hold internal mesa mutexes — killing it
            // would leave them locked. Instead we park indefinitely; the
            // entire old GpuRenderer will be leaked and a fresh one created.
            loop {
                libc::pause();
            }
        }

        // Main thread (or guard not initialized) — forward to real abort().
        let real = REAL_ABORT.load(Ordering::Acquire);
        if !real.is_null() {
            let real_abort: extern "C" fn() -> ! = std::mem::transmute(real);
            real_abort();
        }

        // Last resort: if dlsym failed, use the raw syscall to abort
        let msg = b"[abort-guard] fallback: real abort not resolved, raising SIGABRT\n";
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::raise(libc::SIGABRT);
        libc::_exit(134);
    }
}

/// Force the linker to keep `abort` alive through LTO. `#[used]` ensures this
/// static survives dead-code elimination, and its reference to `abort` keeps
/// the function in the final binary so `--export-dynamic-symbol=abort` can
/// export it to the dynamic symbol table.
#[used]
static ABORT_ANCHOR: unsafe extern "C" fn() -> ! = abort;

/// Re-exec the current process via `execve("/proc/self/exe", ...)`.
///
/// After a GPU context reset, mesa's internal per-device `amdgpu_winsys`
/// singleton retains deadlocked mutexes from the parked worker thread.
/// No new EGL context can be created on the same GPU without deadlocking.
/// The only way to get a clean GPU context is a fresh process.
///
/// `execve` replaces the address space (wiping all mesa state) but keeps
/// the same PID — the session manager doesn't notice the restart, and the
/// gamescope lease socket is still available for a fresh DRM lease.
pub fn self_restart() -> ! {
    unsafe {
        let msg = b"[abort-guard] re-execing process after GPU reset (waiting 2s for GPU to settle)...\n";
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());

        // Give the GPU hardware time to finish resetting. Without this
        // delay, mesa's amdgpu winsys fails to create an IB buffer on
        // the fresh context ("failed to create IB buffer") and segfaults.
        libc::sleep(2);

        // Read our own binary path
        let exe = std::ffi::CString::new("/proc/self/exe").unwrap();

        // Read original argv from /proc/self/cmdline (NUL-separated)
        let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
        let args: Vec<std::ffi::CString> = cmdline
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| std::ffi::CString::new(s).unwrap())
            .collect();
        let argv: Vec<*const libc::c_char> = args
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        // execve replaces the process image entirely — no return on success
        libc::execve(exe.as_ptr(), argv.as_ptr(), std::ptr::null());

        // If execve fails, just exit
        let msg = b"[abort-guard] execve failed, exiting\n";
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::_exit(1);
    }
}
