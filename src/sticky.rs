use std::{
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicPtr, Ordering},
        Arc, PoisonError, RwLock, RwLockReadGuard, TryLockError, TryLockResult,
    },
};

pub struct Sticky<T> {
    ptr: AtomicPtr<Arc<T>>,
    cleared: AtomicBool,
    garbage_collector: RwLock<()>,
}

struct StickyGuard<'a, T> {
    slot: &'a Sticky<T>,
    guard: Option<RwLockReadGuard<'a, ()>>,
}

impl<'a, T> StickyGuard<'a, T> {
    fn new(slot: &'a Sticky<T>) -> TryLockResult<StickyGuard<T>> {
        match slot.garbage_collector.try_read() {
            Ok(guard) => Ok(StickyGuard {
                slot: slot,
                guard: Some(guard),
            }),
            Err(TryLockError::Poisoned(poison)) => {
                Err(TryLockError::Poisoned(PoisonError::new(StickyGuard {
                    slot: slot,
                    guard: Some(poison.into_inner()),
                })))
            }
            Err(TryLockError::WouldBlock) => Err(TryLockError::WouldBlock),
        }
    }
}

impl<'a, T: 'a> Drop for StickyGuard<'a, T> {
    fn drop(&mut self) {
        self.guard = None;
        if self.slot.cleared.load(Ordering::Relaxed) {
            if let Ok(_) = self.slot.garbage_collector.try_write() {
                unsafe {
                    self.slot.clear_backend();
                }
            }
        }
    }
}

impl<T> Sticky<T> {
    pub fn new() -> Sticky<T> {
        Sticky {
            ptr: AtomicPtr::new(null_mut()),
            cleared: AtomicBool::new(false),
            garbage_collector: RwLock::new(()),
        }
    }

    // NOTE: This function never panics and thus cannot poison an `RwLock`
    unsafe fn clear_backend(&self) {
        let ptr = self.ptr.swap(null_mut(), Ordering::Acquire);
        if !ptr.is_null() {
            Box::from_raw(ptr);
        }
    }

    pub fn clear(&self) {
        if self
            .cleared
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            if let Ok(_) = self.garbage_collector.try_write() {
                unsafe {
                    self.clear_backend();
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn stick(&self, val: Arc<T>) -> Option<Arc<T>> {
        if let Ok(guard) = StickyGuard::new(&self) {
            if self.cleared.load(Ordering::Relaxed) {
                return None;
            }
            let ptr = Box::leak(Box::new(val.clone()));
            match self
                .ptr
                .compare_exchange(null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => Some(val),
                Err(old) => {
                    let arc = unsafe { old.as_ref() }.unwrap().clone();
                    std::mem::drop(guard);
                    unsafe { Box::from_raw(ptr) };
                    Some(arc)
                }
            }
        } else {
            None
        }
    }

    pub fn shove(&self, arc: Arc<T>) {
        if let Ok(guard) = StickyGuard::new(&self) {
            if self.cleared.load(Ordering::Relaxed) {
                return;
            }
            let ptr = Box::into_raw(Box::new(arc));
            if self
                .ptr
                .compare_exchange(null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                std::mem::drop(guard);
                unsafe { Box::from_raw(ptr) };
            }
        }
    }

    pub fn get(&self) -> Option<Arc<T>> {
        if let Ok(_) = StickyGuard::new(&self) {
            if self.cleared.load(Ordering::Relaxed) {
                return None;
            }
            match unsafe { self.ptr.load(Ordering::Acquire).as_ref() } {
                Some(arc) => Some(arc.clone()),
                None => None,
            }
        } else {
            None
        }
    }
}

impl<T> Drop for Sticky<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.get_mut();
        if !ptr.is_null() {
            unsafe { Box::from_raw(*ptr) };
        }
    }
}
