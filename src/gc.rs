use crate::trace::Trace;
use crate::collector::{Collector, Soul, LOCAL_SENDER};

use std::sync::RwLock;
use std::sync::{Arc, Weak};
use std::sync::{RwLockReadGuard, RwLockWriteGuard};
use std::sync::mpsc::TrySendError;

pub struct Gc<T: Trace + 'static> {
    item: Arc<Option<RwLock<T>>>
}

pub trait WeakGc: Send {
    fn as_ptr(&self) -> Option<usize>;
}
impl<T: Trace + 'static> WeakGc for Weak<Option<RwLock<T>>> {
    fn as_ptr(&self) -> Option<usize> {
        self.upgrade().as_ref().and_then(|p| Some(Arc::as_ptr(p) as usize))
    }
}

impl<T: Trace> Clone for Gc<T> {
    fn clone(&self) -> Self {
        Gc { item: self.item.clone() }
    }
}

impl<T: Trace> Trace for Gc<T> {
    fn trace(&self, c: &mut Collector) {
        // The collector tries to scan through GC items when collecting cycles.
        // Collectable cycles only happen if a set of strongly connected components
        // are not reachable from the main threads; if, when tracing through items,
        // we can't acquire a read lock (due to something else holding a write lock)
        // then we know that the item is currently in use from the main thread
        // because it is currently accessible and being used. This is assuming
        // that the RwLock guards are never mem::forget'd, however (which is a
        // reasonable assumption to make, imo).
        //
        // Note that tracing through a GC has to clone the Arc while it is trying
        // to acquire the read-lock. This unfortunately will cause spurious deferral
        // by the main threads, if they drop this item while we have a copy.

        // If we can just skip cloning the Arc, do so.
        if self.item.is_none() { return; }

        // Else clone the arc, and try to get a read-lock and visit the item we
        // are pointing to. If the attempt to read-lock failed then we can simply
        // return.
        let tmp = self.item.clone();
        if let Some(i) = &*tmp {
            if let Ok(i) = i.try_read() {
                c.visit(&*i);
            } else {
                // nothing
            }
        }
        // TODO: c.without_defer(tmp); to remove spurious deferrals
    }
}

impl<T: Trace + 'static> Drop for Gc<T> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.item) == 1 {
            // We know we are the only thread to have a reference to this item.
            if Arc::weak_count(&self.item) == 0 {
                // And we know it didn't have a weak count, so wasn't added to
                // the defer list. We just clean up the object entirely.
                // Fall through to the normal Arc drop.
                println!("cleaning up");
            } else {
                // Something has a weak reference to this object; there's a good
                // chance it's the defer list (say, because we had a reference count
                // of two and decremented it twice in quick succession), so we
                // try to remove it from the list.
                println!("reclaiming");
                let ptr = Arc::as_ptr(&self.item);
                LOCAL_SENDER.with(|s| {
                    let rec = s.try_send(Soul::Reclaimed(ptr as usize));
                    match rec {
                        Err(TrySendError::Disconnected(_)) => {
                            unimplemented!("collector thread died?")
                        },
                        Err(TrySendError::Full(_)) => {
                            unimplemented!("collector channel full")
                        },
                        Ok(_) => { },
                    }
                });
            }
        } else {
            // We're dropping an object, and it is possibly the member of a cycle
            // that will keep it alive. Send a Weak<T> pointer to the collector
            // to add to its defer list.
            LOCAL_SENDER.with(|s| {
                let weak =
                    Box::new(Arc::downgrade(&self.item)) as Box<dyn WeakGc + Send>;
                s.try_send(Soul::Died(weak))
            });
        }
        drop(&mut self.item);
    }
}

impl<T: Trace> Gc<T> {
    pub fn new(t: T) -> Self {
        Gc { item: Arc::new(Some(RwLock::new(t))) }
    }

    /// Used internally to create an empty version of a Gc<T>, in order to break cycles
    /// and initialize cyclic testcases.
    pub(crate) fn empty() -> Self {
        Gc { item: Arc::new(None) }
    }

    /// Get a read-only view of the contents of the Gc<T> object. This acquires
    /// and returns the guard for a RwLock read-lock: this means that, if there
    /// is an outstanding write-lock held, it will block until the write-lock is
    /// released.
    ///
    /// If used in the Drop impl of an object contained within a Gc<T>, this method
    /// may panic due to attempting to dereference a Gc pointer that has been nullified
    /// in order to break cycles - however, in normal operations, it will not panic.
    pub fn get(&self) -> RwLockReadGuard<T> {
        self.item.as_ref().as_ref().unwrap().read().unwrap()
    }

    pub fn set<F, O>(&self, f: F) -> O where F: Fn(RwLockWriteGuard<T>) -> O {
        let guard = self.item.as_ref().as_ref().unwrap().write().unwrap();
        f(guard)
    }
}
