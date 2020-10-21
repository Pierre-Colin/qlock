//! This crate provides equivalents to the ones provided by the [`std::sync`] module with the
//! exception that they are guaranteed to offer first-come-first-served fairness. Because their
//! APIs are identical to the ones in [`std::sync`], switching between the two is trivial, and many
//! details in this documentation are omitted.
//!
//! [`std::sync`]: https://doc.rust-lang.org/std/sync/index.html

use std::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
    sync,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    sync::{Arc, LockResult, MutexGuard, PoisonError, TryLockError, TryLockResult},
    thread,
    thread::{panicking, yield_now, ThreadId},
};

mod sticky;

struct QueueNode {
    owner: ThreadId,
    // Values:
    //  0: wait for me
    //  1: ok to read
    //  2: free
    state: AtomicU8,
    pred: sticky::Sticky<QueueNode>,
}

impl QueueNode {
    fn new() -> QueueNode {
        QueueNode {
            owner: thread::current().id(),
            state: AtomicU8::new(0),
            pred: sticky::Sticky::new(),
        }
    }

    fn allow_read(&self) {
        self.state.store(1, Ordering::Release);
    }

    fn unlock(&self) {
        self.state.store(2, Ordering::Release);
    }

    fn can_read(&self) -> bool {
        self.state.load(Ordering::Acquire) >= 1
    }

    fn can_write(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }

    fn is_mine(&self) -> bool {
        self.owner == thread::current().id()
    }
}

/// A first-come-first-served reader-writer lock
///
/// This type of lock allows a number of readers or at most one writer at any point in time. The
/// writer portion of this lock typically allows modification of the underlying data (exclusive
/// access) and the read portion of this lock typically allows for read-only access (shared
/// access).
///
/// In comparison, a [`Mutex`] does not distinguish between readers or writers that acquire the
/// lock, therefore blocking any threads waiting for the lock to become available. An `RwLock`
/// will allow any number of readers to acquire the lock as long as a writer is not holding the lock.
///
/// The type parameter `T` represents the data that this lock protects. It is required that `T`
/// satisfies [`Send`] to be shared across threads and [`Sync`] to allow concurrent access through
/// readers. The RAII guards returned from the locking methods implement [`Deref`] (and
/// [`DerefMut`] for the `write` methods) to allow access to the content of the lock.
///
/// **NOTE**: This type is available only on architectures that support atomic instructions on
/// booleans, 8-bit integers and pointers.
///
/// # First-come-first-served fairness
///
/// The order in which threads attempt to access the lock is identified and access to the data is
/// granted in the same order. For instance, consider the following history:
///
/// 1. Thread A wants shared access: it acquires the lock and enters critical section
/// 2. Thread B wants shared access: it acquires the lock and enters critical section
/// 3. Thread C wants exclusive access: it has to wait until both A and B drop their locks
/// 4. Thread D wants shared access: it has to wait until A, B and C all drop their locks
///
/// In a typical shared lock, thread D could have entered the critical section while threads A and
/// B are still running it, which can considerably delay thread C. In more practical situations, it
/// could also lead to commands being executed in an order that significantly differs from the
/// order in which they were received by a server.
///
/// # Poisoning
///
/// An `RwLock`, like a [`Mutex`], will become poisoned on a panic. Note, however, that an `RwLock`
/// will only be poisoned if a panic occurs while it is locked exclusively (write mode). If a panic
/// occurs in any reader, then the lock will not be poisoned.
///
/// # Examples
///
/// ```
/// use qlock::RwLock;
///
/// let lock = RwLock::new(5);
///
/// // many reader locks can be held at once
/// {
///     let r1 = lock.read().unwrap();
///     let r2 = lock.read().unwrap();
///     assert_eq!(*r1, 5);
///     assert_eq!(*r2, 5);
/// } // read locks are dropped at this point
///
/// // only one write lock may be held, however
/// {
///     let mut w = lock.write().unwrap();
///     *w += 1;
///     assert_eq!(*w, 6);
/// } // write lock is dropped here
/// ```
///  
/// [`std::sync::RwLock`]: https://doc.rust-lang.org/std/sync/struct.RwLock.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
/// [`Sync`]: https://doc.rust-lang.org/std/marker/trait.Sync.html
/// [`Deref`]: https://doc.rust-lang.org/std/ops/trait.Deref.html
/// [`DerefMut`]: https://doc.rust-lang.org/std/ops/trait.DerefMut.html
pub struct RwLock<T: ?Sized> {
    tail: sync::Mutex<Arc<QueueNode>>,
    poisoned: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

/// RAII structure used to release the shared read access of a lock when dropped.
///
/// This structure is created by the `read` and `try_read` methods on `RwLock`.
pub struct RwLockReadGuard<'a, T: ?Sized + 'a> {
    lock: &'a RwLock<T>,
    node: Arc<QueueNode>,
}

/// RAII structure used to release the exclusive write access of a lock when dropped.
///
/// This structure is created by the `write` and `try_write` methods on `RwLock`.
pub struct RwLockWriteGuard<'a, T: ?Sized + 'a> {
    lock: &'a RwLock<T>,
    node: Arc<QueueNode>,
    panic: bool,
}

impl<T> RwLock<T> {
    /// Creates a new instance of an `RwLock<T>` which is unlocked.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let lock = RwLock::new(5);
    /// ```
    pub fn new(t: T) -> RwLock<T> {
        let sentinel = Arc::new(QueueNode::new());
        sentinel.unlock();
        RwLock {
            tail: sync::Mutex::new(sentinel),
            poisoned: AtomicBool::new(false),
            data: UnsafeCell::new(t),
        }
    }
}

impl<T: ?Sized> RwLock<T> {
    fn new_node(&self) -> Arc<QueueNode> {
        let node = Arc::new(QueueNode::new());
        let pred = {
            let mut tail_lock = self.tail.lock().unwrap();
            let old = (*tail_lock).clone();
            *tail_lock = node.clone();
            old
        };
        node.pred.shove(pred);
        node
    }

    fn try_new_node(&self) -> Result<Arc<QueueNode>, TryLockError<MutexGuard<Arc<QueueNode>>>> {
        let node = Arc::new(QueueNode::new());
        let pred = {
            let mut tail_lock = self.tail.try_lock()?;
            let old = (*tail_lock).clone();
            *tail_lock = node.clone();
            old
        };
        node.pred.shove(pred);
        Ok(node)
    }

    /// Locks this `RwLock` with exclusive write access, blocking the current thread until it can
    /// be acquired.
    ///
    /// This function will not return while other writers or other readers currently have access to
    /// the lock.
    ///
    /// Returns an RAII guard which will drop the write access of this `RwLock` when dropped.
    ///
    /// # Errors
    ///
    /// This function returns an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. An error will be returned when
    /// the lock is acquired.
    ///
    /// # Panics
    ///
    /// This function panics when called if the lock is already held by the current thread.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let lock = RwLock::new(1);
    ///
    /// let mut n = lock.write().unwrap();
    /// *n = 2;
    ///
    /// assert!(lock.try_read().is_err());
    /// ```
    pub fn write<'a>(&'a self) -> sync::LockResult<RwLockWriteGuard<'a, T>> {
        let node = self.new_node();
        let mut current = node.pred.get().unwrap();
        loop {
            if current.is_mine() && !current.can_read() {
                node.unlock();
                panic!("this thread already owns a write guard");
            }
            while !current.can_write() {
                yield_now();
            }
            if let Some(next) = current.pred.get() {
                current = next.clone();
            } else {
                let guard = RwLockWriteGuard {
                    lock: self,
                    node: node.clone(),
                    panic: panicking(),
                };
                if self.poisoned.load(Ordering::Relaxed) {
                    return Err(sync::PoisonError::new(guard));
                }
                return Ok(guard);
            }
        }
    }

    /// Locks this `RwLock` with shared access, blocking the current thread until it can be
    /// acquired.
    ///
    /// The calling thread will be blocked until there are no more writers holding the lock and no
    /// more readers which took the lock before a pending writer. There may be other readers
    /// currently inside the lock when this method returns.
    ///
    /// Returns an RAII guard which will release this thread's shared access once it is dropped.
    ///
    /// # Errors
    ///
    /// This function will return an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. The failure will occur
    /// immediately after the lock has been acquired.
    ///
    /// # Panics
    ///
    /// This function panics when called if the lock is already held by the current thread.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::thread;
    ///
    /// use qlock::RwLock;
    ///
    /// let lock = Arc::new(RwLock::new(1));
    /// let c_lock = Arc::clone(&lock);
    ///
    /// let n = lock.read().unwrap();
    /// assert_eq!(*n, 1);
    ///
    /// thread::spawn(move || {
    ///     let r = c_lock.read();
    ///     assert!(r.is_ok());
    /// }).join().unwrap();
    /// ```
    pub fn read<'a>(&'a self) -> sync::LockResult<RwLockReadGuard<'a, T>> {
        let node = self.new_node();
        node.allow_read();
        let mut current = node.pred.get().unwrap();
        loop {
            if current.is_mine() && !current.can_read() {
                node.unlock();
                panic!("this thread already owns a write guard");
            }
            while !current.can_read() {
                yield_now();
            }
            if let Some(next) = current.pred.get() {
                current = next.clone();
            } else {
                let guard = RwLockReadGuard {
                    lock: self,
                    node: node.clone(),
                };
                if self.poisoned.load(Ordering::Relaxed) {
                    return Err(sync::PoisonError::new(guard));
                }
                return Ok(guard);
            }
        }
    }

    /// Attempts to lock this `RwLock` with exclusive write access.
    ///
    /// If the lock could not be acquired at this time, then `Err` is returned. Otherwise, an RAII
    /// guard is returned which will release the lock when it is dropped.
    ///
    /// This function does not block.
    ///
    /// # Errors
    ///
    /// This function will return an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. An error will only be returned if
    /// the lock would have otherwise been acquired.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let lock = RwLock::new(1);
    ///
    /// let n = lock.read().unwrap();
    /// assert_eq!(*n, 1);
    ///
    /// assert!(lock.try_write().is_err());
    /// ```
    pub fn try_write<'a>(&'a self) -> TryLockResult<RwLockWriteGuard<'a, T>> {
        let node = match self.try_new_node() {
            Ok(arc) => arc,
            Err(TryLockError::WouldBlock) => return Err(TryLockError::WouldBlock),
            Err(TryLockError::Poisoned(_)) => unreachable!(),
        };
        let mut current = node.pred.get().unwrap();
        loop {
            if !current.can_write() {
                node.unlock();
                return Err(TryLockError::WouldBlock);
            }
            if let Some(next) = current.pred.get() {
                current = next.clone();
            } else {
                let guard = RwLockWriteGuard {
                    lock: self,
                    node: node.clone(),
                    panic: panicking(),
                };
                if self.poisoned.load(Ordering::Relaxed) {
                    return Err(TryLockError::Poisoned(sync::PoisonError::new(guard)));
                }
                return Ok(guard);
            }
        }
    }

    /// Attempts to acquire this `RwLock` with shared access.
    ///
    /// If the access could not be granted at this time, then `Err` is returned. Otherwise, an RAII
    /// guard is returned which will release the shared access when it is dropped.
    ///
    /// This function does not block.
    ///
    /// # Errors
    ///
    /// This function will return an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. An error will only be returned
    /// if the lock would have been otherwise acquired.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let lock = RwLock::new(1);
    ///
    /// match lock.try_read() {
    ///     Ok(n) => assert_eq!(*n, 1),
    ///     Err(_) => unreachable!(),
    /// };
    /// ```
    pub fn try_read<'a>(&'a self) -> TryLockResult<RwLockReadGuard<'a, T>> {
        let node = match self.try_new_node() {
            Ok(arc) => arc,
            Err(TryLockError::WouldBlock) => return Err(TryLockError::WouldBlock),
            Err(TryLockError::Poisoned(_)) => unreachable!(),
        };
        node.allow_read();
        let mut current = node.pred.get().unwrap();
        loop {
            if !current.can_read() {
                node.unlock();
                return Err(TryLockError::WouldBlock);
            }
            if let Some(next) = current.pred.get() {
                current = next.clone();
            } else {
                let guard = RwLockReadGuard {
                    lock: self,
                    node: node.clone(),
                };
                if self.poisoned.load(Ordering::Relaxed) {
                    return Err(TryLockError::Poisoned(sync::PoisonError::new(guard)));
                }
                return Ok(guard);
            }
        }
    }

    /// Determines whether the lock is poisoned.
    ///
    /// If another thread is active, the lock can still become poisoned at any time. You should not
    /// trust a `false` value for program correctness without additional synchronization.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::thread;
    ///
    /// use qlock::RwLock;
    ///
    /// let lock = Arc::new(RwLock::new(0));
    /// let c_lock = Arc::clone(&lock);
    ///
    /// let _ = thread::spawn(move || {
    ///     let _lock = c_lock.write().unwrap();
    ///     panic!(); // the lock gets poisoned
    /// }).join();
    /// assert_eq!(lock.is_poisoned(), true);
    /// ```
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }

    /// Consumes this `RwLock`, returning the underlying data.
    ///
    /// # Errors
    ///
    /// This function will return an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. An error will only be returned
    /// if the lock would have otherwise been acquired.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let lock = RwLock::new(String::new());
    /// {
    ///     let mut s = lock.write().unwrap();
    ///     *s = "modified".to_owned();
    /// }
    /// assert_eq!(lock.into_inner().unwrap(), "modified");
    /// ```
    pub fn into_inner(mut self) -> LockResult<T>
    where
        T: Sized,
    {
        let x = self.data.into_inner();
        if *self.poisoned.get_mut() {
            Err(PoisonError::new(x))
        } else {
            Ok(x)
        }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `RwLock` mutably, no actual locking needs to take place --
    /// the mutable borrow statically guarantees no lock exists.
    ///
    /// # Errors
    ///
    /// This function will return an error if the `RwLock` is poisoned. An `RwLock` is poisoned
    /// whenever a writer panics while holding an exclusive lock. An error will only be returned
    /// if the lock would have otherwise been acquired.
    ///
    /// # Examples
    ///
    /// ```
    /// use qlock::RwLock;
    ///
    /// let mut lock = RwLock::new(0);
    /// *lock.get_mut().unwrap() = 10;
    /// assert_eq!(*lock.read().unwrap(), 10);
    /// ```
    pub fn get_mut(&mut self) -> sync::LockResult<&mut T> {
        let r = unsafe { &mut *self.data.get() };
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(sync::PoisonError::new(r));
        }
        Ok(r)
    }
}

unsafe impl<T: ?Sized + Sync> Sync for RwLockReadGuard<'_, T> {}

unsafe impl<T: ?Sized + Sync> Sync for RwLockWriteGuard<'_, T> {}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        if !self.panic && panicking() {
            self.lock.poisoned.store(true, Ordering::Relaxed);
        }
        self.node.pred.clear();
        self.node.unlock();
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.node.unlock();
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> RwLock<T> {
        RwLock::new(Default::default())
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(t: T) -> RwLock<T> {
        RwLock::new(t)
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_read() {
            Ok(guard) => f.debug_struct("RwLock").field("data", &&*guard).finish(),
            Err(TryLockError::Poisoned(err)) => f
                .debug_struct("RwLock")
                .field("data", &&*err.get_ref())
                .finish(),
            Err(TryLockError::WouldBlock) => {
                struct LockedPlaceholder;
                impl fmt::Debug for LockedPlaceholder {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("<locked>")
                    }
                }

                f.debug_struct("RwLock")
                    .field("data", &LockedPlaceholder)
                    .finish()
            }
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLockReadGuard")
            .field("lock", &self.lock)
            .finish()
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLockWriteGuard")
            .field("lock", &self.lock)
            .finish()
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}
