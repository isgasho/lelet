use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::forget;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::process::abort;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// Call [`abort`] when `f` panic
///
/// [`abort`]: https://doc.rust-lang.org/std/process/fn.abort.html
#[inline(always)]
pub fn abort_on_panic(f: impl FnOnce()) {
    struct Bomb;

    impl Drop for Bomb {
        #[inline(always)]
        fn drop(&mut self) {
            abort();
        }
    }

    let bomb = Bomb;

    f();

    forget(bomb);
}

/// Defer the execution until the scope is done
#[macro_export]
macro_rules! defer {
  ($($body:tt)*) => {
      let _guard = {
          struct Guard<F: FnOnce()>(Option<F>);

          impl<F: FnOnce()> Drop for Guard<F> {
            #[inline(always)]
            fn drop(&mut self) {
                  (self.0).take().map(|f| f());
              }
          }

          Guard(Some(|| {
              let _: () = { $($body)* };
          }))
      };
  };
}

/// Extracts the successful type of a `Poll<T>`.
///
/// This macro bakes in propagation of `Pending` signals by returning early.
#[macro_export]
macro_rules! ready {
    ($e:expr $(,)?) => {
        match $e {
            Poll::Ready(t) => t,
            Poll::Pending => return Poll::Pending,
        }
    };
}

/// Future that will yield multiple times
#[derive(Debug)]
pub struct Yields(pub usize);

impl Future for Yields {
    type Output = ();

    #[inline(always)]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        if self.0 == 0 {
            Poll::Ready(())
        } else {
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A simple lock.
///
/// Intentionally I don't povide `lock`, you can spin loop `try_lock` if you want.
/// You should use [`Mutex`] if you need blocking lock.
///
/// [`Mutex`]: https://doc.rust-lang.org/std/sync/struct.Mutex.html
pub struct SimpleLock<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for SimpleLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SimpleLock<T> {}

impl<T> SimpleLock<T> {
    /// Returns a new SimpleLock initialized with `value`.
    #[inline(always)]
    pub fn new(value: T) -> SimpleLock<T> {
        SimpleLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized + Default> Default for SimpleLock<T> {
    #[inline(always)]
    fn default() -> SimpleLock<T> {
        SimpleLock::new(T::default())
    }
}

impl<T: ?Sized> SimpleLock<T> {
    /// Try to lock.
    #[inline(always)]
    pub fn try_lock(&self) -> Option<SimpleLockGuard<T>> {
        if self.locked.swap(true, Ordering::Acquire) {
            None
        } else {
            Some(SimpleLockGuard {
                parent: self,
                _marker: PhantomData,
            })
        }
    }

    /// Is locked ?
    #[inline(always)]
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SimpleLock<T> {
    #[inline(always)]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => f.debug_tuple("SimpleLock").field(&&*guard).finish(),
            None => f.write_str("SimpleLock(<locked>)"),
        }
    }
}

/// A guard holding a [`SimpleLock`].
///
/// [`SimpleLock`]: struct.SimpleLock.html
pub struct SimpleLockGuard<'a, T: 'a + ?Sized> {
    parent: &'a SimpleLock<T>,

    // !Send + !Sync
    _marker: PhantomData<*mut ()>,
}

impl<T: ?Sized> Drop for SimpleLockGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.parent.locked.store(false, Ordering::Release);
    }
}

impl<T: ?Sized> Deref for SimpleLockGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.parent.value.get() }
    }
}

impl<T: ?Sized> DerefMut for SimpleLockGuard<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.parent.value.get() }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SimpleLockGuard<'_, T> {
    #[inline(always)]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// Block current thread until f is complete
#[inline(always)]
pub fn block_on<F: Future>(mut f: F) -> F::Output {
    // originally copied from `extreme` (https://docs.rs/extreme)

    #[allow(clippy::mutex_atomic)]
    #[derive(Default)]
    struct Parker(Mutex<bool>, Condvar);

    #[allow(clippy::mutex_atomic)]
    impl Parker {
        #[inline(always)]
        fn unpark(self: &Parker) {
            *self.0.lock().unwrap() = true;
            self.1.notify_one();
        }

        #[inline(always)]
        fn park(self: &Parker) {
            let mut runnable = self.0.lock().unwrap();
            while !*runnable {
                runnable = self.1.wait(runnable).unwrap();
            }
            *runnable = false;
        }
    }

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        //
        // clone: unsafe fn(*const ()) -> RawWaker
        #[inline(always)]
        |parker| unsafe {
            let parker = Arc::from_raw(parker as *const Parker);
            let cloned_parker = parker.clone();
            forget(parker);
            RawWaker::new(Arc::into_raw(cloned_parker) as *const (), &VTABLE)
        },
        //
        // wake: unsafe fn(*const ())
        #[inline(always)]
        |parker| unsafe { Arc::from_raw(parker as *const Parker).unpark() },
        //
        // wake_by_ref: unsafe fn(*const ())
        #[inline(always)]
        |parker| unsafe { (&*(parker as *const Parker)).unpark() },
        //
        // drop: unsafe fn(*const ())
        #[inline(always)]
        |parker| unsafe { drop(Arc::from_raw(parker as *const Parker)) },
    );

    let parker = Arc::new(Parker::default());

    let waker = unsafe {
        Waker::from_raw(RawWaker::new(
            Arc::into_raw(parker.clone()) as *const (),
            &VTABLE,
        ))
    };

    let mut f = unsafe { Pin::new_unchecked(&mut f) };
    let mut cx = Context::from_waker(&waker);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Pending => parker.park(),
            Poll::Ready(val) => return val,
        }
    }
}
