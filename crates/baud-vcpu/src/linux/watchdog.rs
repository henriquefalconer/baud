// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// The real wall-clock watchdog (todo.md §14.1 "Still open" item 1, docs/determinism.md "Known
// limits" §4): the one non-deterministic intervention in the whole system, deliberately outside
// the deterministic boundary. A guest driven by `run_until_halted` with no periodic-timer engine
// wired in (baud-multiverse's other `run_to_first_halt_with_*` entry points already carry their
// own deterministic `max_exits`/`max_ticks` budget — not the gap this closes) can make *zero* VM
// exits at all under this project's subtractive machine model (no APIC, no PIT, no host
// interrupts): a tight `jmp $` loop never traps. So the only way to reclaim a vCPU thread parked
// forever inside one blocking `KVM_RUN` ioctl is to force it to return, from a companion thread,
// via a real POSIX signal.
//
// This is architecturally different from `pmu.rs`'s abandoned PMU-overflow-signal approach (see
// that module's doc): a PMU overflow is a *guest-visible* interrupt whose delivery is gated on
// this host's (WSL2-on-Hyper-V) imperfect nested virtualization of the performance-monitoring
// interrupt vector, and was found to be silently dropped while the physical core sits in VMX
// non-root mode. A signal sent to the vCPU thread via `pthread_kill` instead uses the general
// Linux "kick a running task" mechanism — the kernel raises `TIF_SIGPENDING` on the target task
// and, if it is currently executing (in guest mode or not), sends it a reschedule IPI; the IPI
// itself is an external interrupt, and KVM always exits unconditionally on a host external
// interrupt arriving during guest execution, at which point it notices the pending signal and
// returns `-EINTR`. This path is independent of any guest-visible interrupt controller and is how
// every real VMM (QEMU, Firecracker, crosvm) implements exactly this kind of watchdog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Real-time signal repurposed to interrupt a blocking `KVM_RUN`. Nothing else in this workspace
/// installs a signal handler or sends a signal to a thread (grepped clean across `crates/` before
/// choosing this), so the number is free and this module owns it exclusively.
const WATCHDOG_SIGNAL: libc::c_int = libc::SIGUSR1;

/// Install a real (not `SIG_IGN`) handler for [`WATCHDOG_SIGNAL`], once per process. A blocking
/// syscall is only interrupted (`EINTR`) by a signal actually delivered to a registered handler
/// without `SA_RESTART` — `SIG_IGN` discards the signal before it can interrupt anything, and the
/// POSIX default disposition for `SIGUSR1` is to terminate the process, which a watchdog firing
/// against a guest that happens to finish a heartbeat later must never do.
fn ensure_handler_installed() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        extern "C" fn noop(_: libc::c_int) {}
        // SAFETY: `action` is a plain C struct filled in below with only well-defined values
        // (a zeroed `sigset_t` immediately replaced by `sigemptyset`, a real function pointer,
        // and `0` flags); `sigaction` with a non-null `act` and null `oldact` is always safe to
        // call and only ever mutates process-global signal disposition state, which is exactly
        // what this function exists to do, guarded by `Once` so it runs exactly once.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = noop as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0; // deliberately no SA_RESTART: the interrupted ioctl must return EINTR
            let rc = libc::sigaction(WATCHDOG_SIGNAL, &action, std::ptr::null_mut());
            assert_eq!(rc, 0, "failed to install the watchdog's SIGUSR1 handler");
        }
    });
}

/// A companion thread that kills a spinning vCPU thread after `budget` of real wall-clock time —
/// exactly the "supervisor's wall-clock watchdog (outside the deterministic boundary)"
/// `docs/determinism.md`'s "Known limits" §4 promises. Armed for the duration of one
/// `run_until_halted` call and always [`disarm`](Self::disarm)d before that call returns, on
/// every path (halted, a real `DeterminismHole`, or the watchdog's own kill) — a late-firing
/// signal must never land in whatever unrelated work the vCPU thread does next. This is a real
/// hazard, not a hypothetical one: `baud-server` runs boots on `tokio::task::spawn_blocking`'s
/// reusable thread pool, so a stray pending signal on an OS thread that outlives this call could
/// interrupt a totally unrelated future task scheduled onto the same thread.
pub(super) struct Watchdog {
    done: Arc<(Mutex<bool>, Condvar)>,
    /// Set by the watchdog thread itself, strictly before it calls `pthread_kill` — the vCPU
    /// thread only ever reads this after observing `EINTR`, and signal delivery is asynchronous
    /// but never *reordered before* the syscall that sends it, so by the time that `EINTR` is
    /// observable this is guaranteed to already be `true` if it was this watchdog that caused it.
    pub(super) fired: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Arm a new watchdog targeting the *calling* thread (`libc::pthread_self()`, captured here
    /// so a caller cannot accidentally arm one against the wrong thread). `budget.is_zero()`
    /// disables the watchdog entirely (no thread spawned, `fired` can never become `true`) — the
    /// same "0 disables" convention `crates/baud-multiverse/src/lib.rs`'s simulated
    /// `quantum_limit_ms` already uses.
    pub(super) fn arm(budget: Duration) -> Self {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let fired = Arc::new(AtomicBool::new(false));
        if budget.is_zero() {
            return Watchdog { done, fired, handle: None };
        }
        ensure_handler_installed();
        // SAFETY: `pthread_self` has no preconditions and always succeeds.
        let target = unsafe { libc::pthread_self() };
        let done2 = Arc::clone(&done);
        let fired2 = Arc::clone(&fired);
        let handle = thread::spawn(move || {
            let (lock, cvar) = &*done2;
            let mut guard = lock.lock().expect("watchdog mutex poisoned");
            let deadline = Instant::now() + budget;
            loop {
                if *guard {
                    return; // disarmed: the guest reached Hlt/Shutdown before the budget ran out
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                // `wait_timeout` can wake spuriously before `deadline`; looping back to recheck
                // both `*guard` and the remaining time (rather than trusting a single wait) is
                // what makes this correct in that case instead of just usually correct.
                guard = cvar.wait_timeout(guard, deadline - now).expect("watchdog mutex poisoned").0;
            }
            if !*guard {
                fired2.store(true, Ordering::SeqCst);
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "wall-clock watchdog killed a spinning guest (docs/determinism.md \"Known \
                     limits\" §4: the one non-deterministic intervention in this system — logged, \
                     not replayed)"
                );
                // SAFETY: `target` is this watchdog's own vCPU thread, alive for the whole
                // `run_until_halted` call this `Watchdog` is scoped to — that call only returns
                // (and only then `disarm`s and drops this `Watchdog`) after this very thread has
                // already been joined, so `target` cannot have exited yet.
                unsafe {
                    libc::pthread_kill(target, WATCHDOG_SIGNAL);
                }
            }
        });
        Watchdog { done, fired, handle: Some(handle) }
    }

    /// Cancel a still-pending watchdog and wait for its thread to actually exit — called
    /// unconditionally by `run_until_halted` right before it returns, on every path, so no
    /// watchdog thread ever outlives the call that armed it (see this struct's own doc for why
    /// that matters on a reused thread pool).
    pub(super) fn disarm(self) {
        {
            let (lock, cvar) = &*self.done;
            *lock.lock().expect("watchdog mutex poisoned") = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.handle {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No real `/dev/kvm` needed for any test in this module — [`Watchdog`] only ever touches
    /// threads, a mutex/condvar, and a signal, never a `VcpuFd`.

    #[test]
    fn zero_budget_disables_the_watchdog_entirely() {
        let watchdog = Watchdog::arm(Duration::ZERO);
        assert!(watchdog.handle.is_none(), "a zero budget must not spawn a watchdog thread at all");
        std::thread::sleep(Duration::from_millis(50));
        assert!(!watchdog.fired.load(Ordering::SeqCst), "a disabled watchdog must never fire");
        watchdog.disarm();
    }

    #[test]
    fn fires_after_its_budget_elapses_with_nothing_disarming_it() {
        let watchdog = Watchdog::arm(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(300)); // comfortably past the budget
        let fired = Arc::clone(&watchdog.fired);
        // `disarm` joins the watchdog thread -- by the time it returns, that thread has
        // already either fired (the case under test) or exited having found `done` already
        // true, so `fired`'s value is stable and race-free to read right after this.
        watchdog.disarm();
        assert!(fired.load(Ordering::SeqCst), "watchdog must fire once its budget elapses");
    }

    #[test]
    fn disarming_before_the_budget_elapses_prevents_it_from_ever_firing() {
        let watchdog = Watchdog::arm(Duration::from_secs(5));
        let fired = Arc::clone(&watchdog.fired);
        watchdog.disarm(); // long before the 5s budget would elapse
        assert!(!fired.load(Ordering::SeqCst), "disarming in time must prevent it from ever firing");
    }
}
