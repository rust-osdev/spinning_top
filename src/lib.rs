//! Provides a simple spinlock based on the abstractions provided by the [`lock_api`] crate.
//! 
//! [`lock_api`]: https://docs.rs/lock_api/

#![no_std]
#![warn(missing_docs)]
#![warn(missing_debug_implementations)]

pub use lock_api;
pub use spinlock::{RawSpinlock, Spinlock, SpinlockGuard};

mod spinlock;
