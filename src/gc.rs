use crate::trace::Trace;
use crate::collector::{Collector, Soul, LOCAL_SENDER, SyncSender};

use std::error::Error;

#[cfg(not(all(feature = "shuttle", test)))]
pub use std::{thread, sync::{atomic::*, Arc, Weak, RwLock, RwLockReadGuard, RwLockWriteGuard}};
#[cfg(all(feature = "shuttle", test))]
pub use shuttle::{thread, sync::{atomic::*, Arc, Weak, RwLock, RwLockReadGuard, RwLockWriteGuard}};

use crate::tracker::{Tracker, TrackerLocation};

lazy_static::lazy_static! {
    static ref LIVE_COUNT: TrackerLocation = {
        TrackerLocation::new(AtomicUsize::new(0))
    };
}

pub fn number_of_live_objects() -> usize {
    LIVE_COUNT.get()
}

#[derive(Debug)]
pub struct Gc<T: Trace + 'static> {
    // The item needs an Option so we can null out objects to break cycles,
    // an RwLock so that we acquire read-locks from the collector without blocking
    // in most cases, and an AtomicUsize so we can properly handle [Graph tearing]
    // when collecting.
    item: Arc<(RwLock<Option<T>>, AtomicUsize, Tracker)>
}

pub trait GcObject: Send + Sync {
    fn read_and_trace(&self, _root: &WeakRoot, c: &mut Collector) -> bool;
    fn flags(&self) -> Option<GcFlags>;
    fn mark_visited(&self);
    fn clear_visited(&self);
    fn invalidate(&self);
}

#[derive(Clone)]
pub struct WeakRoot(pub(crate) Weak<dyn GcObject>);

impl WeakRoot {
    pub fn visit(&self, c: &mut Collector) -> bool {
        // When we visit a WeakGc object from our defer list root, we have to
        // acquire a read-lock. This is because there could be a sequence
        // of A->B->C->D references from our root A to another Gc<T> D, and
        // we have to be able to visit through to D without it being deallocated
        // in the meanwhile.
        // While we hold a read-lock no mutator thread is able to acquire a new
        // write-lock! This will occasionally pause mutator threads, but is
        // still better than a full stop-the-world phase. We also simply immediately
        // skip tracing a root if we can't acquire a read-lock, which means
        // a mutator thread is currently holding a write-lock and thus the object
        // is definitely still reachable.
        let Some(upgraded) = self.0.upgrade() else {
            // we couldn't upgrade the weak reference, so it was a dead root object
            println!("visited dead weakgc");
            return false;
        };
        upgraded.read_and_trace(self, c)
    }

    pub fn as_ptr(&self) -> *const () {
        self.0.as_ptr() as *const ()
    }

    pub fn flags(&self) -> Option<GcFlags> {
        println!("FLAGS");
        //self.0.upgrade().map(|s| s.0.load(Ordering::Acquire) ).and_then(GcFlags::from_bits)
        self.0.upgrade().and_then(|s| s.flags())
    }

    pub fn mark_visited(&self) {
        println!("MARK VISIT {:x}", self.0.as_ptr() as *const () as usize);
        //self.0.upgrade().map(|s| s.0.store(GcFlags::VISITED.bits(), Ordering::Release));
        self.0.upgrade().map(|s| { println!("MARK COULD UPGRADE"); s.mark_visited() });
    }

    pub fn clear_visited(&self) {
        println!("CLEAR VISIT");
        //self.0.upgrade().map(|s| s.0.store(GcFlags::NONE.bits(), Ordering::Release));
        self.0.upgrade().map(|s| s.clear_visited());
    }

    pub fn invalidate(&self) {
        println!("INVALIDATE");
        //self.0.upgrade().map(|s| s.2.invalidate() );
        self.0.upgrade().map(|s| s.invalidate());
    }
}

impl<T: Trace> GcObject for (RwLock<Option<T>>, AtomicUsize, Tracker) {
    fn read_and_trace(&self, _self: &WeakRoot, c: &mut Collector) -> bool {
        // When tracing, we have to acquire a *write* lock if the object has
        // interior mutability - See [Moving Reference] for more.
        if <T as Trace>::MUT == true {
            // TODO: ugh this is annoying to format. ill write it later.
            panic!("{} MUT", std::any::type_name::<T>());
        }
        let Ok(r) = self.0.try_read() else {
            println!("can't acquire read-lock on weakgc");
            // We couldn't acquire a read-lock, so we know *something* else has
            // an outstanding lock. The collector doesn't hold guards, so it must
            // be a mutator, which means the object is still reachable.
            return false;
        };
        let flag = if let Some(reader) = r.as_ref() {
            // We were able to acquire a read-lock, so we know it doesn't have a
            // write-lock outstanding. Now we visit the object to find all reachable
            // Gc<T> objects to add to our worklist.
            println!("started tracing weakgc 0x{:x}", _self.as_ptr() as usize);
            reader.trace(_self, c);
            true
        } else {
            // we got a read-lock, but the object is None - this should only
            // happen if we already broke a cycle somehow.
            println!("empty weakgc");
            false
        };
        // these drops matter, so make them explicit: in particular upgraded being
        // dropped may cause T::drop() to be called, since we could upgrade a weak and then
        // drop the upgraded Arc after the mutator drops its last reference.
        drop(r);
        flag
    }

    fn flags(&self) -> Option<GcFlags> {
        GcFlags::from_bits(self.1.load(Ordering::Acquire))
    }

    fn mark_visited(&self) {
        self.1.store(GcFlags::VISITED.bits(), Ordering::Release);
    }

    fn clear_visited(&self) {
        self.1.store(GcFlags::NONE.bits(), Ordering::Release);
    }

    fn invalidate(&self) {
        self.0.try_write().unwrap().take();
    }
}

impl<T: Trace> Clone for Gc<T> {
    fn clone(&self) -> Self {
        println!("GC CLONE 0x{:x}", self.as_ptr() as usize);
        Gc { item: self.item.clone() }
    }
}

impl<T: Trace> Trace for Gc<T> where (RwLock<Option<T>>, AtomicUsize, Tracker): GcObject {
    fn trace(&self, root: &WeakRoot, c: &mut Collector) {
        // We only trace Gc<T> objects as objects reachable from a WeakGc root,
        // never as the entry-point.
        // if the root is the same as a reachable object, it's just a self-loop.
        if Arc::as_ptr(&self.item) as usize == root.0.as_ptr() as *const () as usize {
            unimplemented!("what do we do here?")
        }

        // Else get a weak reference to the object, and add it to the graph and
        // an edge from root->self.
        if (self.item.1.load(Ordering::Acquire) & GcFlags::VISITED.bits()) == 0 {
            // TODO: this could probably be more efficient - there's no reason
            // to make a weak ref if the item is already in the node map...
            c.add_node(&WeakRoot(Arc::downgrade(&self.item) as Weak<_> as Weak<dyn GcObject>));
        }
        c.add_edge(root, WeakRoot(Arc::downgrade(&self.item) as Weak<_> as Weak<dyn GcObject>));
    }
}

// Note [Graph Tearing]
// TODO: this is outdated, fix it
// Unfortunately, when doing [Cycle Collection], we hit a problem in the face of concurrent mutators.
// Consider the simple graph of items A<->B that form a cyclic reference and are
// both on our defer list, and assume there is a mutator with a live reference
// to A. When we visit A for the first time we initialize it with a value=2 (one
// from B, one from the mutator). If we didn't have concurrent mutators we could
// visit B (which we initialize and then decrement value=1->0) and then visit A
// again (which we decrement value=2->1), and then we are done with visiting.
// We correctly see that A still has a value=1, and thus the cycle is live.
//
// But if we have concurrent mutators, we could instead get the following ordering:
// We visit A for the first time and initialize it with value=2 (one from B,
// one from the mutrator). The mutator then uses its live reference to run
// `A.read().B.write().link = A.clone()` - that is, it adds an additional edge
// from B->A, so that it now has *two* internal edges. When we advance to B we then
// visit A twice, decrementing value=2->1->0, and end up erroneously considering
// the cycle dead. The issue is that our view of the graph was torn due to not
// visiting all nodes atomically, which pausing mutators is equivalent to: when we
// initialize the node value to `strong_count`, by the time we visit all nodes
// that value may be stale.
//
// Luckily there is a fix. When considering nodes for SCC subgraphs of value=0
// items, we *could* double-check that `strong_count` is still the same as when
// we started: if it is greater an additional edge was created, which has to have
// been done by a mutator, and thus the object (and SCC) is still alive.
//
// We also have to handle the mutator both creating *and then removing* an edge,
// however! The additional edge could have been added for B->A, and then removed
// before we double-check items; in that case the `strong_count` value is still
// the same, but we still double-counted. This lends us the *actual* implementation
// that we use: we add an AtomicUsize to all Gc<T> objects, which provide a bitfield
// of flags. When we visit an object, the first thing we do is set a VISITED bit
// on the object. If a mutator drops an object, if it has a VISITED bit it sets
// it to (VISITED | DIRTY). When we scan objects to make sure they have value=0
// we then also discount them if `shared_count` isn't the original value, *or*
// if it has DIRTY set in its bitfield.
// This fixes the graph tearing problem: when we double check either
// 1) the mutator is currently holding a live reference it added, in which case
// `strong_count` is more than incoming-count 2) it added and then removed a
// reference, in which case `strong_count` is correct but DIRTY was set.

// Note [Moving Reference]
// On some mutator operations read/write operations we have to transition from
// VISITED->DIRTY as well, not just on Drop. We could have an object graph like
// A<->B<->C with a live mutator reference to B. If the collector visits A, and
// then the mutator does B.get(|b| b.A.set(|a| b.C.set(|c| c.B = Some(a.B.take()) )))
// to move a reference from A to C, and then the collector visits C, it would
// incorrectly double-count the reference to B and think the subgraph of objects
// is unreachable. The problem is one of *moving* a reference across the collector's
// visitation frontier; if the collector only *clones* the reference then we handle
// it via [Graph Tearing].
// We always have to transition on Gc.set(f) calls. We *also* have to transition
// on Gc.get(f) calls, however, if the Gc object has interior mutability that would
// allow for moving a Gc<T> object through a &-reference. Tracing::MUT is an associated
// constant that must be correctly set by implementers to tell us if an object needs
// that or not.
// Finally, we have to care about a Gc references moving *within* an object as well:
// if we have struct Foo { a: Mutex<Option<Gc<Bar>>, b: Mutex<Option<Gc<Bar>> },
// we have to make sure Foo.get(|foo| *foo.b.lock() = Some(foo.a.lock().take()) )
// happening concurrently with visiting Foo is handled correctly. The Gc.get(f)
// transition doesn't fix this problem since the visit could start after the mutator
// passes the flag check - instead, if we are visiting a Tracing::MUT object, we
// try to acquire a *write-lock* instead of only a *read-lock*, guaranteeing us
// an exclusive reference for the duration (or allowing us to bail out if any
// existing locks exist).


bitflags::bitflags! {
    pub struct GcFlags: usize {
        const NONE = 0b00000000;
        const VISITED = 0b00000001;
        const DIRTY = 0b00000010;
    }
}

// shuttle doesn't have a channel try_send method. we don't want to block the
// mutator threads when trying to send Reclaimed events, since the worst case
// is just the collector doing some extra work and fail to upgrade a dead Weak<T>
// instead of blocking user code.
fn try_send<T: Send + 'static>(chan: &SyncSender<T>, val: T) -> Result<(), Box<dyn Error>> {
    #[cfg(not(all(feature = "shuttle", test)))]
    {
        chan.try_send(val).map_err(|e| Box::new(e) as Box<dyn Error>)
    }
    #[cfg(all(feature = "shuttle", test))]
    {
        chan.send(val).map_err(|e| Box::new(e) as Box<dyn Error>)
    }
}

impl<T: Trace + 'static> Drop for Gc<T> {
    fn drop(&mut self) {
        if crate::collector::IS_COLLECTOR.with(|b|{ b.load(Ordering::Acquire) }) {
            // the collector thread upgrades and downgrades its weak handles a lot,
            // and they should never be added to the deferred list.
            println!("dropping gc on collector thread");
            return drop(&mut self.item);
        }
        // See [Defer List]
        if Arc::strong_count(&self.item) <= 1 {
            // We know we are the only thread to have a reference to this item.
            if Arc::weak_count(&self.item) != 0 {
                // Something has a weak reference to this object; there's a good
                // chance it's the defer list (say, because we had a reference count
                // of two and decremented it twice in quick succession), so we
                // try to remove it from the list.
                // (The weak count could be because we sent the item to the collector
                // while it was running a cycle collector, and thus the Weak<T>
                // is buffered in the channel and we can't remove it until the
                // collector finishes - there's probably some weird cache we could
                // do to mitigate that if it's a problem e.g. while collection
                // is happening add/remove to a per-cpu hashmap instead)
                println!("reclaiming soul");
                let ptr = Arc::as_ptr(&self.item);
                LOCAL_SENDER.with(|s| {
                    let rec = s.chan.borrow().as_ref().unwrap().send(Soul::Reclaimed(ptr as usize));
                    rec.unwrap();

                    //rec.map_err(|e| unimplemented!("sending soul error {:?}", e))
                });
            } else {
                if Arc::strong_count(&self.item) != 1 { panic!() }
                // We know it didn't have a weak count, so wasn't added to
                // the defer list. We just clean up the object entirely.
                // Fall through to the normal Arc drop.
                println!("immediate free");
            }
        } else {
            // We're dropping an object, and it is possibly the member of a cycle
            // that will keep it alive. Send a Weak<T> pointer to the collector
            // to add to its defer list.
            //
            // If we have an AtomicUsize that has the VISITED bit set, we need to
            // set (VISITED | DIRTY) in order to mitigate [Graph Tearing]
            self.visited_to_dirty();
            // we don't return - even thought we marked it dirty so it isn't
            // collected this cycle, we have to buffer it on the channel
            // so that it is a candidate for collection *next* cycle.
            LOCAL_SENDER.with(|s| {
                let weak = Arc::downgrade(&self.item);
                // this may block, blocking mutator thread! this can happen if
                // the collector thread is processing too many recvs from too many
                // threads and can't keep up with bandwidth, or if its busy doing
                // a collection and collectors fill up its work queue to process
                // before it finishes.
                // some of this could be made better by queuing work per-thread
                // that we can clear via Reclaimed if the collector is doing a
                // collection.
                s.chan.borrow().as_ref().unwrap().send(Soul::Died(WeakRoot(weak)))
            });
        }
        drop(&mut self.item)
    }
}

impl<T: Trace> Gc<T> {
    pub fn new(t: T) -> Self {
        Gc { item: Arc::new((RwLock::new(Some(t)), AtomicUsize::new(GcFlags::NONE.bits()), Tracker::of(&LIVE_COUNT))) }
    }

    fn visited_to_dirty(&self) {
        // TODO: think about atomic orderings
        // does this even need a load+cmpxchg instead of only cmpxchg??
        if GcFlags::from_bits(self.item.1.load(Ordering::Acquire)).unwrap().contains(GcFlags::VISITED) {
            let res = self.item.1.compare_exchange(
                GcFlags::VISITED.bits(),
                (GcFlags::DIRTY | GcFlags::VISITED).bits(),
                Ordering::Acquire, // TODO: think about atomic orderings
                Ordering::Relaxed);
            if let Err(actual) = res {
                println!("gc drop cmpxchg failed compare, was instead {:?}", GcFlags::from_bits(actual));
            }
        }
    }


    /// Used internally to create an empty version of a Gc<T>, in order to break cycles
    /// and initialize cyclic testcases.
    pub(crate) fn empty() -> Self {
        Gc { item: Arc::new((RwLock::new(None), AtomicUsize::new(GcFlags::NONE.bits()), Tracker::of(&LIVE_COUNT))) }
    }

    /// Get a read-only view of the contents of the Gc<T> object. This acquires
    /// and returns the guard for a RwLock read-lock: this means that, if there
    /// is an outstanding write-lock held, it will block until the write-lock is
    /// released.
    ///
    /// If used in the Drop impl of an object contained within a Gc<T>, this method
    /// may panic due to attempting to dereference a Gc pointer that has been nullified
    /// in order to break cycles - however, in normal operations, it will not panic.
    pub fn get<F, O>(&self, f: F) -> O where F: Fn(&T) -> O {
        println!("get on 0x{:x}", self.as_ptr() as usize);
        // See [Moving Reference]
        if <Self as Trace>::MUT {
            //self.visited_to_dirty();
        }
        f(self.item.0.read().unwrap().as_ref().unwrap())
    }

    pub fn set<F, O>(&self, f: F) -> O where F: Fn(&mut T) -> O {
        let mut binding = self.item.0.write().unwrap();
        let locked = binding.as_mut().unwrap();
        // See [Moving Reference]
        //self.visited_to_dirty();
        f(locked)
    }

    pub fn as_ptr(&self) -> *const () {
        Arc::as_ptr(&self.item) as *const _ as *const _
    }
}
