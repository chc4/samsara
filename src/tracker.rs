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

#[cfg(test)]
#[derive(Debug)]
pub struct Tracker {
    loc: &'static TrackerLocation
}

#[cfg(not(test))]
#[derive(Debug)]
pub struct Tracker();

#[cfg(test)]
impl Drop for Tracker {
    fn drop(&mut self) {
        // atomically decrement object count
        Self::lose(self.loc);
    }
}

#[cfg(test)]
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
            match loc.val.compare_exchange(old, old + 1, Ordering::Acquire, Ordering::Relaxed) {
                Err(curr) => { old = curr; continue; },
                Ok(new) => { assert_eq!(old, new); break },
            };
        }
    }

    fn lose(loc: &'static TrackerLocation) {
        let mut old = loc.val.load(Ordering::Acquire);
        loop {
            match loc.val.compare_exchange(old, old - 1, Ordering::Acquire, Ordering::Relaxed) {
                Err(curr) => { old = curr; continue; },
                Ok(new) => { assert_eq!(old, new); break },
            };
        }
    }
}

#[cfg(not(test))]
impl Tracker {
    pub fn of(loc: &'static TrackerLocation) -> Self {
        Tracker()
    }

    pub fn get(&self) -> usize {
        0
    }

    fn gain(loc: &'static TrackerLocation) {
    }
    fn lose(loc: &'static TrackerLocation) {
    }
}




