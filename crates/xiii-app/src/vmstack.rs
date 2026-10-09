//! The script VM's host-stack budget.
//!
//! The engine's interpreter consumes host stack for every script call. The measured cost of
//! one interpreted frame is ~4.4 KiB in a debug build, and the VM's call-depth guard follows
//! the engine's own limit of 250 interpreted calls (Core.dll `UObject::ProcessInternal`
//! compares the runaway counter against `0xFA` at VA 0x101166e0). 250 x ~4.4 KiB is ~1.1 MiB,
//! which overflows a **1 MiB** main-thread stack before the guard fires — the constrained case
//! is the binary's main thread (the Windows GUI-subsystem default), so the `--play` loop, the
//! `--survey` child and the headless `--play-script` helper run their work on this explicit
//! stack instead. Test threads already get the ~2 MiB default (`RUST_MIN_STACK`), which fits
//! the guarded maximum with ~2x margin; they do not need this wrapper
//! (`vm_tests::recursion_guard_fits_a_2mib_stack` pins that margin).

/// 64 MiB leaves ~50x margin over the guarded maximum plus nested native re-entry.
pub const VM_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Runs `f` on a thread with an explicit [`VM_STACK_SIZE`] stack and joins it. Panics in `f`
/// resume in the caller; scoped lifetimes allow borrowing inputs (scene, params).
pub fn run_on_vm_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(VM_STACK_SIZE)
            .spawn_scoped(s, f)
            .expect("spawn the VM host-stack thread")
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}
