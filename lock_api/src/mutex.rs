// Copyright 2018 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::mem;
use core::ops::{Deref, DerefMut};

#[cfg(feature = "owning_ref")]
use owning_ref::StableAddress;

#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Basic operations for a mutex.
///
/// Types implementing this trait can be used by `Mutex` to form a safe and
/// fully-functioning mutex type.
///
/// # Safety
///
/// Implementations of this trait must ensure that the mutex is actually
/// exclusive: a lock can't be acquired while the mutex is already locked.
pub unsafe trait RawMutex {
    /// Initial value for an unlocked mutex.
    const INIT: Self;

    /// Marker type which determines whether a lock guard should be `Send`. Use
    /// one of the `GuardSend` or `GuardNoSend` helper types here.
    type GuardMarker;

    /// Acquires this mutex, blocking the current thread until it is able to do so.
    fn lock(&self);

    /// Attempts to acquire this mutex without blocking.
    fn try_lock(&self) -> bool;

    /// Unlocks this mutex.
    fn unlock(&self);
}

/// Additional methods for mutexes which support fair unlocking.
///
/// Fair unlocking means that a lock is handed directly over to the next waiting
/// thread if there is one, without giving other threads the opportunity to
/// "steal" the lock in the meantime. This is typically slower than unfair
/// unlocking, but may be necessary in certain circumstances.
pub unsafe trait RawMutexFair: RawMutex {
    /// Unlocks this mutex using a fair unlock protocol.
    fn unlock_fair(&self);

    /// Temporarily yields the mutex to a waiting thread if there is one.
    ///
    /// This method is functionally equivalent to calling `unlock_fair` followed
    /// by `lock`, however it can be much more efficient in the case where there
    /// are no waiting threads.
    fn bump(&self) {
        self.unlock_fair();
        self.lock();
    }
}

/// Additional methods for mutexes which support locking with timeouts.
///
/// The `Duration` and `Instant` types are specified as associated types so that
/// this trait is usable even in `no_std` environments.
pub unsafe trait RawMutexTimed: RawMutex {
    /// Duration type used for `try_lock_for`.
    type Duration;

    /// Instant type used for `try_lock_until`.
    type Instant;

    /// Attempts to acquire this lock until a timeout is reached.
    fn try_lock_for(&self, timeout: Self::Duration) -> bool;

    /// Attempts to acquire this lock until a timeout is reached.
    fn try_lock_until(&self, timeout: Self::Instant) -> bool;
}

/// A mutual exclusion primitive useful for protecting shared data
///
/// This mutex will block threads waiting for the lock to become available. The
/// mutex can also be statically initialized or created via a `new`
/// constructor. Each mutex has a type parameter which represents the data that
/// it is protecting. The data can only be accessed through the RAII guards
/// returned from `lock` and `try_lock`, which guarantees that the data is only
/// ever accessed when the mutex is locked.
pub struct Mutex<R: RawMutex, T: ?Sized> {
    raw: R,
    data: UnsafeCell<T>,
}

// Copied and modified from serde
#[cfg(feature = "serde")]
impl<R, T> Serialize for Mutex<R, T>
where
    R: RawMutex,
    T: Serialize + ?Sized,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.lock().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, R, T> Deserialize<'de> for Mutex<R, T>
where
    R: RawMutex,
    T: Deserialize<'de> + ?Sized,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer).map(Mutex::new)
    }
}

unsafe impl<R: RawMutex + Send, T: ?Sized + Send> Send for Mutex<R, T> {}
unsafe impl<R: RawMutex + Sync, T: ?Sized + Send> Sync for Mutex<R, T> {}

impl<R: RawMutex, T> Mutex<R, T> {
    /// Creates a new mutex in an unlocked state ready for use.
    #[cfg(feature = "nightly")]
    #[inline]
    pub const fn new(val: T) -> Mutex<R, T> {
        Mutex { data: UnsafeCell::new(val), raw: R::INIT }
    }

    /// Creates a new mutex in an unlocked state ready for use.
    #[cfg(not(feature = "nightly"))]
    #[inline]
    pub fn new(val: T) -> Mutex<R, T> {
        Mutex { data: UnsafeCell::new(val), raw: R::INIT }
    }

    /// Consumes this mutex, returning the underlying data.
    #[inline]
    #[allow(unused_unsafe)]
    pub fn into_inner(self) -> T {
        unsafe { self.data.into_inner() }
    }
}

impl<R: RawMutex, T: ?Sized> Mutex<R, T> {
    #[inline]
    fn guard(&self) -> MutexGuard<'_, R, T> {
        MutexGuard { mutex: self, marker: PhantomData }
    }

    /// Acquires a mutex, blocking the current thread until it is able to do so.
    ///
    /// This function will block the local thread until it is available to acquire
    /// the mutex. Upon returning, the thread is the only thread with the mutex
    /// held. An RAII guard is returned to allow scoped unlock of the lock. When
    /// the guard goes out of scope, the mutex will be unlocked.
    ///
    /// Attempts to lock a mutex in the thread which already holds the lock will
    /// result in a deadlock.
    #[inline]
    pub fn lock(&self) -> MutexGuard<'_, R, T> {
        self.raw.lock();
        self.guard()
    }

    /// Attempts to acquire this lock.
    ///
    /// If the lock could not be acquired at this time, then `None` is returned.
    /// Otherwise, an RAII guard is returned. The lock will be unlocked when the
    /// guard is dropped.
    ///
    /// This function does not block.
    #[inline]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, R, T>> {
        if self.raw.try_lock() { Some(self.guard()) } else { None }
    }

    /// Returns a mutable reference to the underlying data.
    ///
    /// Since this call borrows the `Mutex` mutably, no actual locking needs to
    /// take place---the mutable borrow statically guarantees no locks exist.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }

    /// Forcibly unlocks the mutex.
    ///
    /// This is useful when combined with `mem::forget` to hold a lock without
    /// the need to maintain a `MutexGuard` object alive, for example when
    /// dealing with FFI.
    ///
    /// # Safety
    ///
    /// This method must only be called if the current thread logically owns a
    /// `MutexGuard` but that guard has be discarded using `mem::forget`.
    /// Behavior is undefined if a mutex is unlocked when not locked.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        self.raw.unlock();
    }

    /// Returns the underlying raw mutex object.
    ///
    /// Note that you will most likely need to import the `RawMutex` trait from
    /// `lock_api` to be able to call functions on the raw mutex.
    ///
    /// # Safety
    ///
    /// This method is unsafe because it allows unlocking a mutex while
    /// still holding a reference to a `MutexGuard`.
    #[inline]
    pub unsafe fn raw(&self) -> &R {
        &self.raw
    }
}

impl<R: RawMutexFair, T: ?Sized> Mutex<R, T> {
    /// Forcibly unlocks the mutex using a fair unlock procotol.
    ///
    /// This is useful when combined with `mem::forget` to hold a lock without
    /// the need to maintain a `MutexGuard` object alive, for example when
    /// dealing with FFI.
    ///
    /// # Safety
    ///
    /// This method must only be called if the current thread logically owns a
    /// `MutexGuard` but that guard has be discarded using `mem::forget`.
    /// Behavior is undefined if a mutex is unlocked when not locked.
    #[inline]
    pub unsafe fn force_unlock_fair(&self) {
        self.raw.unlock_fair();
    }
}

impl<R: RawMutexTimed, T: ?Sized> Mutex<R, T> {
    /// Attempts to acquire this lock until a timeout is reached.
    ///
    /// If the lock could not be acquired before the timeout expired, then
    /// `None` is returned. Otherwise, an RAII guard is returned. The lock will
    /// be unlocked when the guard is dropped.
    #[inline]
    pub fn try_lock_for(&self, timeout: R::Duration) -> Option<MutexGuard<'_, R, T>> {
        if self.raw.try_lock_for(timeout) { Some(self.guard()) } else { None }
    }

    /// Attempts to acquire this lock until a timeout is reached.
    ///
    /// If the lock could not be acquired before the timeout expired, then
    /// `None` is returned. Otherwise, an RAII guard is returned. The lock will
    /// be unlocked when the guard is dropped.
    #[inline]
    pub fn try_lock_until(&self, timeout: R::Instant) -> Option<MutexGuard<'_, R, T>> {
        if self.raw.try_lock_until(timeout) { Some(self.guard()) } else { None }
    }
}

impl<R: RawMutex, T: ?Sized + Default> Default for Mutex<R, T> {
    #[inline]
    fn default() -> Mutex<R, T> {
        Mutex::new(Default::default())
    }
}

impl<R: RawMutex, T> From<T> for Mutex<R, T> {
    #[inline]
    fn from(t: T) -> Mutex<R, T> {
        Mutex::new(t)
    }
}

impl<R: RawMutex, T: ?Sized + fmt::Debug> fmt::Debug for Mutex<R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_struct("Mutex").field("data", &&*guard).finish(),
            None => {
                struct LockedPlaceholder;
                impl fmt::Debug for LockedPlaceholder {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("<locked>")
                    }
                }

                f.debug_struct("Mutex").field("data", &LockedPlaceholder).finish()
            }
        }
    }
}

/// An RAII implementation of a "scoped lock" of a mutex. When this structure is
/// dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be accessed through this guard via its
/// `Deref` and `DerefMut` implementations.
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct MutexGuard<'a, R: RawMutex, T: ?Sized> {
    mutex: &'a Mutex<R, T>,
    marker: PhantomData<(&'a mut T, R::GuardMarker)>,
}

unsafe impl<'a, R: RawMutex + Sync + 'a, T: ?Sized + Sync + 'a> Sync for MutexGuard<'a, R, T> {}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> MutexGuard<'a, R, T> {
    /// Returns a reference to the original `Mutex` object.
    pub fn mutex(s: &Self) -> &'a Mutex<R, T> {
        s.mutex
    }

    /// Makes a new `MappedMutexGuard` for a component of the locked data.
    ///
    /// This operation cannot fail as the `MutexGuard` passed
    /// in already locked the mutex.
    ///
    /// This is an associated function that needs to be
    /// used as `MutexGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn map<U: ?Sized, F>(s: Self, f: F) -> MappedMutexGuard<'a, R, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let raw = &s.mutex.raw;
        let data = f(unsafe { &mut *s.mutex.data.get() });
        mem::forget(s);
        MappedMutexGuard { raw, data, marker: PhantomData }
    }

    /// Attempts to make  a new `MappedMutexGuard` for a component of the
    /// locked data. The original guard is return if the closure returns `None`.
    ///
    /// This operation cannot fail as the `MutexGuard` passed
    /// in already locked the mutex.
    ///
    /// This is an associated function that needs to be
    /// used as `MutexGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn try_map<U: ?Sized, F>(s: Self, f: F) -> Result<MappedMutexGuard<'a, R, U>, Self>
    where
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let raw = &s.mutex.raw;
        let data = match f(unsafe { &mut *s.mutex.data.get() }) {
            Some(data) => data,
            None => return Err(s),
        };
        mem::forget(s);
        Ok(MappedMutexGuard { raw, data, marker: PhantomData })
    }

    /// Temporarily unlocks the mutex to execute the given function.
    ///
    /// This is safe because `&mut` guarantees that there exist no other
    /// references to the data protected by the mutex.
    #[inline]
    pub fn unlocked<F, U>(s: &mut Self, f: F) -> U
    where
        F: FnOnce() -> U,
    {
        s.mutex.raw.unlock();
        defer!(s.mutex.raw.lock());
        f()
    }
}

impl<'a, R: RawMutexFair + 'a, T: ?Sized + 'a> MutexGuard<'a, R, T> {
    /// Unlocks the mutex using a fair unlock protocol.
    ///
    /// By default, mutexes are unfair and allow the current thread to re-lock
    /// the mutex before another has the chance to acquire the lock, even if
    /// that thread has been blocked on the mutex for a long time. This is the
    /// default because it allows much higher throughput as it avoids forcing a
    /// context switch on every mutex unlock. This can result in one thread
    /// acquiring a mutex many more times than other threads.
    ///
    /// However in some cases it can be beneficial to ensure fairness by forcing
    /// the lock to pass on to a waiting thread if there is one. This is done by
    /// using this method instead of dropping the `MutexGuard` normally.
    #[inline]
    pub fn unlock_fair(s: Self) {
        s.mutex.raw.unlock_fair();
        mem::forget(s);
    }

    /// Temporarily unlocks the mutex to execute the given function.
    ///
    /// The mutex is unlocked a fair unlock protocol.
    ///
    /// This is safe because `&mut` guarantees that there exist no other
    /// references to the data protected by the mutex.
    #[inline]
    pub fn unlocked_fair<F, U>(s: &mut Self, f: F) -> U
    where
        F: FnOnce() -> U,
    {
        s.mutex.raw.unlock_fair();
        defer!(s.mutex.raw.lock());
        f()
    }

    /// Temporarily yields the mutex to a waiting thread if there is one.
    ///
    /// This method is functionally equivalent to calling `unlock_fair` followed
    /// by `lock`, however it can be much more efficient in the case where there
    /// are no waiting threads.
    #[inline]
    pub fn bump(s: &mut Self) {
        s.mutex.raw.bump();
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> Deref for MutexGuard<'a, R, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> DerefMut for MutexGuard<'a, R, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> Drop for MutexGuard<'a, R, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.raw.unlock();
    }
}

impl<'a, R: RawMutex + 'a, T: fmt::Debug + ?Sized + 'a> fmt::Debug for MutexGuard<'a, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'a, R: RawMutex + 'a, T: fmt::Display + ?Sized + 'a> fmt::Display for MutexGuard<'a, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(feature = "owning_ref")]
unsafe impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> StableAddress for MutexGuard<'a, R, T> {}

/// An RAII mutex guard returned by `MutexGuard::map`, which can point to a
/// subfield of the protected data.
///
/// The main difference between `MappedMutexGuard` and `MutexGuard` is that the
/// former doesn't support temporarily unlocking and re-locking, since that
/// could introduce soundness issues if the locked object is modified by another
/// thread.
#[must_use = "if unused the Mutex will immediately unlock"]
pub struct MappedMutexGuard<'a, R: RawMutex, T: ?Sized> {
    raw: &'a R,
    data: *mut T,
    marker: PhantomData<&'a mut T>,
}

unsafe impl<'a, R: RawMutex + Sync + 'a, T: ?Sized + Sync + 'a> Sync
    for MappedMutexGuard<'a, R, T>
{
}
unsafe impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> Send for MappedMutexGuard<'a, R, T> where
    R::GuardMarker: Send
{
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> MappedMutexGuard<'a, R, T> {
    /// Makes a new `MappedMutexGuard` for a component of the locked data.
    ///
    /// This operation cannot fail as the `MappedMutexGuard` passed
    /// in already locked the mutex.
    ///
    /// This is an associated function that needs to be
    /// used as `MappedMutexGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn map<U: ?Sized, F>(s: Self, f: F) -> MappedMutexGuard<'a, R, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let raw = s.raw;
        let data = f(unsafe { &mut *s.data });
        mem::forget(s);
        MappedMutexGuard { raw, data, marker: PhantomData }
    }

    /// Attempts to make  a new `MappedMutexGuard` for a component of the
    /// locked data. The original guard is return if the closure returns `None`.
    ///
    /// This operation cannot fail as the `MappedMutexGuard` passed
    /// in already locked the mutex.
    ///
    /// This is an associated function that needs to be
    /// used as `MappedMutexGuard::map(...)`. A method would interfere with methods of
    /// the same name on the contents of the locked data.
    #[inline]
    pub fn try_map<U: ?Sized, F>(s: Self, f: F) -> Result<MappedMutexGuard<'a, R, U>, Self>
    where
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let raw = s.raw;
        let data = match f(unsafe { &mut *s.data }) {
            Some(data) => data,
            None => return Err(s),
        };
        mem::forget(s);
        Ok(MappedMutexGuard { raw, data, marker: PhantomData })
    }
}

impl<'a, R: RawMutexFair + 'a, T: ?Sized + 'a> MappedMutexGuard<'a, R, T> {
    /// Unlocks the mutex using a fair unlock protocol.
    ///
    /// By default, mutexes are unfair and allow the current thread to re-lock
    /// the mutex before another has the chance to acquire the lock, even if
    /// that thread has been blocked on the mutex for a long time. This is the
    /// default because it allows much higher throughput as it avoids forcing a
    /// context switch on every mutex unlock. This can result in one thread
    /// acquiring a mutex many more times than other threads.
    ///
    /// However in some cases it can be beneficial to ensure fairness by forcing
    /// the lock to pass on to a waiting thread if there is one. This is done by
    /// using this method instead of dropping the `MutexGuard` normally.
    #[inline]
    pub fn unlock_fair(s: Self) {
        s.raw.unlock_fair();
        mem::forget(s);
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> Deref for MappedMutexGuard<'a, R, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.data }
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> DerefMut for MappedMutexGuard<'a, R, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.data }
    }
}

impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> Drop for MappedMutexGuard<'a, R, T> {
    #[inline]
    fn drop(&mut self) {
        self.raw.unlock();
    }
}

impl<'a, R: RawMutex + 'a, T: fmt::Debug + ?Sized + 'a> fmt::Debug for MappedMutexGuard<'a, R, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<'a, R: RawMutex + 'a, T: fmt::Display + ?Sized + 'a> fmt::Display
    for MappedMutexGuard<'a, R, T>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(feature = "owning_ref")]
unsafe impl<'a, R: RawMutex + 'a, T: ?Sized + 'a> StableAddress for MappedMutexGuard<'a, R, T> {}
