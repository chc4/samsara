use crate::trace::Trace;

#[cfg(not(all(feature = "shuttle", test)))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(all(feature = "shuttle", test))]
use shuttle::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub struct TrackerLocation { val: AtomicUsize }

impl TrackerLocation {
    pub fn new(val: AtomicUsize) -> Self {
        Self { val }
    }

    pub fn get(&self) -> usize {
        self.val.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct Tracker {
    loc: &'static TrackerLocation
}

impl Drop for Tracker {
    fn drop(&mut self) {
        // atomically decrement object count
        Self::lose(self.loc);
    }
}

impl Tracker {
    pub fn of(loc: &'static TrackerLocation) -> Self {
        Self::gain(loc);
        Self { loc }
    }

    pub fn get(&self) -> usize {
        self.loc.get()
    }

    fn gain(loc: &'static TrackerLocation) {
        let mut old = loc.val.load(Ordering::Acquire);
        loop {
            let Ok(new) = loc.val.compare_exchange(old, old + 1, Ordering::Acquire, Ordering::Relaxed) else {
                continue;
            };
            if new == old {
                break;
            } else {
                old = new;
            }
        }
    }

    fn lose(loc: &'static TrackerLocation) {
        let mut old = loc.val.load(Ordering::Acquire);
        loop {
            let Ok(new) = loc.val.compare_exchange(old, old - 1, Ordering::Acquire, Ordering::Relaxed) else {
                continue;
            };
            if new == old {
                break;
            } else {
                old = new;
            }
        }
    }
}



