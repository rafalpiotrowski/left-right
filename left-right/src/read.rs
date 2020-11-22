use std::cell::Cell;
use std::sync::atomic;
use std::sync::atomic::AtomicPtr;
use std::sync::{self, Arc};
use std::{fmt, mem};

mod guard;
pub use guard::ReadGuard;

pub struct ReadHandle<T> {
    pub(crate) inner: sync::Arc<AtomicPtr<T>>,
    pub(crate) epochs: crate::Epochs,
    epoch: sync::Arc<sync::atomic::AtomicU64>,
    epoch_i: usize,
    my_epoch: Cell<u64>,
    enters: Cell<usize>,
}

impl<T> Drop for ReadHandle<T> {
    fn drop(&mut self) {
        // epoch must already be even for us to have &mut self,
        // so okay to lock since we're not holding up the epoch anyway.
        let e = self.epochs.lock().unwrap().remove(self.epoch_i);
        assert!(Arc::ptr_eq(&e, &self.epoch));
        assert_eq!(self.enters.get(), 0);
    }
}

impl<T> fmt::Debug for ReadHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadHandle")
            .field("epochs", &self.epochs)
            .field("epoch", &self.epoch)
            .field("my_epoch", &self.my_epoch)
            .finish()
    }
}

impl<T> Clone for ReadHandle<T> {
    fn clone(&self) -> Self {
        ReadHandle::new_with_arc(
            sync::Arc::clone(&self.inner),
            sync::Arc::clone(&self.epochs),
        )
    }
}

impl<T> ReadHandle<T> {
    pub(crate) fn new(inner: T, epochs: crate::Epochs) -> Self {
        let store = Box::into_raw(Box::new(inner));
        let inner = sync::Arc::new(AtomicPtr::new(store));
        Self::new_with_arc(inner, epochs)
    }

    fn new_with_arc(inner: Arc<AtomicPtr<T>>, epochs: crate::Epochs) -> Self {
        // tell writer about our epoch tracker
        let epoch = sync::Arc::new(atomic::AtomicU64::new(0));
        // okay to lock, since we're not holding up the epoch
        let epoch_i = epochs.lock().unwrap().insert(Arc::clone(&epoch));

        Self {
            epochs,
            epoch,
            epoch_i,
            my_epoch: Cell::new(0),
            enters: Cell::new(0),
            inner,
        }
    }
}

impl<T> ReadHandle<T> {
    /// Take out a guarded live reference to the read side of the `T`.
    ///
    /// While the reference lives, the `T` cannot be refreshed.
    ///
    /// If the `T` has been destroyed, this function returns `None`.
    pub fn enter(&self) -> Option<ReadGuard<'_, T>> {
        let enters = self.enters.get();
        if enters != 0 {
            // We have already locked the epoch.
            // Just give out another guard.
            let r_handle = self.inner.load(atomic::Ordering::Acquire);
            // since we previously bumped our epoch, this pointer will remain valid until we bump
            // it again, which only happens when the last ReadGuard is dropped.
            let r_handle = unsafe { r_handle.as_ref() };

            return if let Some(r_handle) = r_handle {
                self.enters.set(enters + 1);
                Some(ReadGuard {
                    handle: guard::ReadHandleState::from(self),
                    epoch: self.my_epoch.get(),
                    t: r_handle,
                })
            } else {
                unreachable!("if pointer is null, no ReadGuard should have been issued");
            };
        }

        // once we update our epoch, the writer can no longer do a swap until we set the MSB to
        // indicate that we've finished our read. however, we still need to deal with the case of a
        // race between when the writer reads our epoch and when they decide to make the swap.
        //
        // assume that there is a concurrent writer. it just swapped the atomic pointer from A to
        // B. the writer wants to modify A, and needs to know if that is safe. we can be in any of
        // the following cases when we atomically swap out our epoch:
        //
        //  1. the writer has read our previous epoch twice
        //  2. the writer has already read our previous epoch once
        //  3. the writer has not yet read our previous epoch
        //
        // let's discuss each of these in turn.
        //
        //  1. since writers assume they are free to proceed if they read an epoch with MSB set
        //     twice in a row, this is equivalent to case (2) below.
        //  2. the writer will see our epoch change, and so will assume that we have read B. it
        //     will therefore feel free to modify A. note that *another* pointer swap can happen,
        //     back to A, but then the writer would be block on our epoch, and so cannot modify
        //     A *or* B. consequently, using a pointer we read *after* the epoch swap is definitely
        //     safe here.
        //  3. the writer will read our epoch, notice that MSB is not set, and will keep reading,
        //     continuing to observe that it is still not set until we finish our read. thus,
        //     neither A nor B are being modified, and we can safely use either.
        //
        // in all cases, using a pointer we read *after* updating our epoch is safe.

        // so, update our epoch tracker.
        let epoch = self.my_epoch.get() + 1;
        self.my_epoch.set(epoch);
        self.epoch.store(epoch, atomic::Ordering::Release);

        // ensure that the pointer read happens strictly after updating the epoch
        atomic::fence(atomic::Ordering::SeqCst);

        // then, atomically read pointer, and use the map being pointed to
        let r_handle = self.inner.load(atomic::Ordering::Acquire);

        // since we bumped our epoch, this pointer will remain valid until we bump it again
        let r_handle = unsafe { r_handle.as_ref() };

        if let Some(r_handle) = r_handle {
            // add a guard to ensure we restore read parity even if we panic
            let enters = self.enters.get() + 1;
            self.enters.set(enters);
            Some(ReadGuard {
                handle: guard::ReadHandleState::from(self),
                epoch,
                t: r_handle,
            })
        } else {
            // the map has been destroyed, so restore parity and return None
            self.epoch.store(
                epoch | 1 << (mem::size_of_val(&self.my_epoch) * 8 - 1),
                atomic::Ordering::Release,
            );
            None
        }
    }

    /// Returns true if the writer has destroyed this `T`.
    ///
    /// See [`WriteHandle::destroy`].
    pub fn is_destroyed(&self) -> bool {
        self.inner.load(atomic::Ordering::Acquire).is_null()
    }
}
