//! Interrupt-safe spinlocks.
//!
//! The kernel is small enough that a test-and-set lock is the right tool. The
//! lock masks IRQs on the local core before spinning, so a core cannot take an
//! interrupt that tries to acquire the same lock and deadlock against itself.
//! Cross-core contention is handled by the atomic with acquire/release
//! ordering; no lock-free algorithm is used anywhere.
//!
//! `force_unlock` exists only for the panic path, where the panicking core may
//! already own a lock it will never release.

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::aarch64::{irq_restore, irq_save_disable};

pub struct IrqSpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: access to `data` is serialized by `locked`.
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}
// SAFETY: moving the lock moves exclusive ownership of `data`.
unsafe impl<T: Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let flags = irq_save_disable();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        IrqSpinLockGuard { lock: self, flags }
    }

    // Non-blocking acquisition is part of the lock API even though the 0.0.3
    // fast paths always use the blocking `lock`.
    #[allow(dead_code)]
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let flags = irq_save_disable();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqSpinLockGuard { lock: self, flags })
        } else {
            irq_restore(flags);
            None
        }
    }

    /// Release the lock without restoring DAIF. Only the panic path should use
    /// this, and only after it has established that it is the owner.
    pub fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    flags: u64,
}

impl<T> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        irq_restore(self.flags);
    }
}
