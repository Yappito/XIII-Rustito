//! The script VM's host-stack budget.
//!
//! The engine's interpreter consumes host stack for every script call. The measured cost of
//! one interpreted frame is ~4.4 KiB in a debug build, and the VM's call-depth guard follows
//! the engine's own limit of 250 interpreted calls (Core.dll `UObject::ProcessInternal`
//! compares the runaway counter against `0xFA` at VA 0x101166e0). 250 x ~4.4 KiB is ~1.1 MiB,
//! which overflows a 1 MiB main-thread stack before the guard fires, so every VM-driving
//! entry point (the `--play` loop, the `--survey` child, the headless `--play-script` helper
//! and the test harness) runs its work on this explicit stack instead.

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
