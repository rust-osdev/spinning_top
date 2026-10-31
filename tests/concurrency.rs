//! Concurrency tests for the lock implementations.
//!
//! These tests also pass under a normal `cargo test`, but they are written to
//! be run under Miri, which is what the `miri` CI job does:
//!
//! - Miri's data race detector reports any two unsynchronized accesses to the
//!   protected data where one of them is a write. This catches guards that
//!   wrongly overlap, but also missing `Acquire`/`Release` orderings in the
//!   lock: without them, accesses in consecutive critical sections are not
//!   synchronized, even if they never actually overlap.
//! - With a high `-Zmiri-preemption-rate`, Miri switches threads at the end
//!   of many basic blocks, so threads also interleave between the atomic
//!   operations inside a single lock call (as in the race fixed in #32).
//! - Each seed (`-Zmiri-many-seeds`) produces a different, reproducible thread
//!   schedule and a different choice of values for weak memory loads.
//!
//! To run the tests like CI does:
//!
//! ```sh
//! MIRIFLAGS="-Zmiri-many-seeds=0..64 -Zmiri-preemption-rate=0.5" \
//!     cargo +nightly miri test --all-features --test concurrency -- --test-threads=1
//! ```

use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use spinning_top::lock_api::{Mutex, RwLock, RwLockUpgradableReadGuard, RwLockWriteGuard};
use spinning_top::relax::{Backoff, Relax, Spin};
use spinning_top::{RawRwSpinlock, RawSpinlock, RwSpinlock};

/// Four threads, so that every pair of the 6 `rwlock` operations below runs in the
/// same iteration on some pair of threads: their starting operations differ by
/// 1, 2 and 3. It also lets several threads wait on another one at the same
/// time.
const THREADS: usize = 4;
/// Enough for every thread to run every operation of the `rwlock` test at
/// least once, while keeping each Miri run short.
const ITERATIONS: usize = 10;

#[test]
fn mutex_spin() {
    mutex::<Spin>();
}

#[test]
fn mutex_backoff() {
    mutex::<Backoff>();
}

#[test]
fn rwlock_spin() {
    rwlock::<Spin>();
}

#[test]
fn rwlock_backoff() {
    rwlock::<Backoff>();
}

/// Regression test for <https://github.com/rust-osdev/spinning_top/pull/32>.
///
/// A failed `try_upgradable_read` used to set the upgradable flag and then
/// clear it again. If `downgrade_to_upgradable` ran in between, it cleared the
/// flag of the downgraded guard, leaving the lock unlocked while the guard
/// still existed.
///
/// The `rwlock` tests can find this too, but only with the right schedule.
/// This test sets up the racy situation directly. It only uses `try_*`
/// methods, which never wait, so the relax strategy doesn't matter.
#[test]
fn failed_try_upgradable_read_during_downgrade_to_upgradable() {
    // Each iteration is one more chance to hit the bad interleaving.
    for _ in 0..ITERATIONS {
        let lock = RwSpinlock::new(0);
        // Starts the contender right when the main thread downgrades, so that
        // the two are as likely as possible to interleave.
        let start = Barrier::new(2);
        let writer = lock.write();

        thread::scope(|s| {
            let contender = s.spawn(|| {
                start.wait();
                // The main thread holds a write guard, then an upgradable
                // guard, so both attempts must fail. With the bug, the second
                // one could succeed after the flag was cleared.
                assert!(lock.try_upgradable_read().is_none());
                assert!(lock.try_write().is_none());
            });

            start.wait();
            let guard = RwLockWriteGuard::downgrade_to_upgradable(writer);
            contender.join().unwrap();

            // With the bug, the lock state is now 0 although `guard` still
            // exists, so this `try_write` succeeds.
            assert!(lock.try_write().is_none());
            drop(guard);
            assert!(lock.try_write().is_some());
        });
    }
}

/// Generic over the relax strategy, because it changes how the blocking
/// methods wait. `Send + Sync` is needed to share the lock between threads.
fn mutex<R: Relax + Send + Sync>() {
    let lock = Mutex::<RawSpinlock<R>, usize>::new(0);

    run_threads(|thread| {
        for i in 0..ITERATIONS {
            // Use both ways of acquiring the lock, which run different code:
            // `lock` spins on a weak compare-exchange, `try_lock` uses a single
            // strong compare-exchange. If `try_lock` fails because another
            // thread holds the lock, fall back to `lock`, so that every
            // iteration increments the counter exactly once.
            let mut guard = if (thread + i) % 2 == 0 {
                lock.lock()
            } else {
                lock.try_lock().unwrap_or_else(|| lock.lock())
            };
            *guard += 1;
        }
    });

    // Fails if two guards overlapped and an increment got lost.
    assert_eq!(lock.into_inner(), THREADS * ITERATIONS);
}

/// Generic over the relax strategy, see [`mutex`].
fn rwlock<R: Relax + Send + Sync>() {
    let lock = RwLock::<RawRwSpinlock<R>, [usize; 2]>::new([0; 2]);
    let upgradable = UpgradableHeld::default();

    run_threads(|thread| {
        for i in 0..ITERATIONS {
            // Every thread cycles through the 6 operations below, each thread
            // starting at a different one, so that different operations run
            // concurrently.
            match (thread + i) % 6 {
                // shared locking
                0 => check(&lock.read()),
                // exclusive locking
                1 => modify(&mut lock.write()),
                // An upgradable guard that is simply dropped.
                2 => {
                    let guard = lock.upgradable_read();
                    upgradable.acquired();
                    check(&guard);
                    upgradable.released();
                }
                // Downgrading a write guard to a read guard. The read guard
                // must still see the write.
                3 => {
                    let mut writer = lock.write();
                    modify(&mut writer);
                    check(&RwLockWriteGuard::downgrade(writer));
                }
                // All transitions of an upgradable guard: upgrade it, write,
                // downgrade back to upgradable, then downgrade to a read guard.
                4 => {
                    let guard = lock.upgradable_read();
                    upgradable.acquired();
                    check(&guard);
                    upgradable.released();
                    let mut writer = RwLockUpgradableReadGuard::upgrade(guard);
                    modify(&mut writer);
                    let guard = RwLockWriteGuard::downgrade_to_upgradable(writer);
                    upgradable.acquired();
                    check(&guard);
                    upgradable.released();
                    check(&RwLockUpgradableReadGuard::downgrade(guard));
                }
                // The non-blocking variants. They often fail here because other
                // threads hold the lock, and a failed attempt must leave the lock
                // state unchanged. #32 was a bug in exactly this case.
                _ => {
                    if let Some(reader) = lock.try_read() {
                        check(&reader);
                    }
                    if let Some(mut writer) = lock.try_write() {
                        modify(&mut writer);
                    }
                    if let Some(guard) = lock.try_upgradable_read() {
                        upgradable.acquired();
                        check(&guard);
                        upgradable.released();
                        // Fails while readers exist. Then the upgradable guard
                        // is returned in the `Err` and dropped here.
                        if let Ok(mut writer) = RwLockUpgradableReadGuard::try_upgrade(guard) {
                            modify(&mut writer);
                        }
                    }
                }
            }
        }
    });

    check(&lock.into_inner());
}

/// Runs `f(thread_index)` on `THREADS` threads and waits for all of them.
///
/// Scoped threads can borrow the lock from the caller's stack, so no `Arc` is
/// needed. A panic in any thread is propagated when the scope ends.
fn run_threads(f: impl Fn(usize) + Sync) {
    thread::scope(|s| {
        for thread in 0..THREADS {
            let f = &f;
            s.spawn(move || f(thread));
        }
    });
}

/// Readers check that both elements are equal, writers increment both. If a
/// reader overlaps with a writer, the reader may see them differ, even
/// without Miri.
fn check(data: &[usize; 2]) {
    assert_eq!(data[0], data[1]);
}

fn modify(data: &mut [usize; 2]) {
    data[0] += 1;
    data[1] += 1;
}

/// Detects two upgradable guards existing at the same time.
///
/// Upgradable guards only read the data, so Miri's data race detector cannot
/// notice two of them overlapping. Instead, every holder sets this flag.
///
/// To avoid false alarms, set the flag only after acquiring an upgradable
/// guard and clear it before giving the guard up (by dropping, upgrading or
/// downgrading it).
///
/// The flag uses `Relaxed` on purpose. The swap still always sees the latest
/// value, because it is a read-modify-write operation. Stronger orderings would
/// make the flag synchronize the threads itself, which could hide a missing
/// `Acquire` or `Release` in the lock.
#[derive(Default)]
struct UpgradableHeld(AtomicBool);

impl UpgradableHeld {
    fn acquired(&self) {
        let already_held = self.0.swap(true, Ordering::Relaxed);
        assert!(!already_held, "two upgradable guards at the same time");
    }

    fn released(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
