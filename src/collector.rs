use crate::trace::Trace;
use crate::gc::{Gc, WeakGc};

use std::cell::RefCell;
use std::mem;
use std::sync::{Arc, Weak, Mutex, RwLock, Condvar};
// TODO: see if crossbeam mpmc is faster than std mpsc?
use std::sync::mpsc::{Sender, Receiver, sync_channel, SyncSender};
use std::thread::JoinHandle;

use std::collections::{HashSet, HashMap};

#[derive(Default)]
pub struct Collector {
    deferred: HashSet<Box<dyn WeakGc + 'static>>,
    pub visited: HashMap<usize, (usize, usize)>, // ptr -> (starting strong count, incoming edges)
}

// Note [Defer List]
// When a Gc<T> is dropped, it either has a `strong_count` of 1 or >1. If it is
// simply 1, then we know that we were the last live reference to the object, and
// thus it isn't part of a cycle. If it is >1, then it *may* be part of a cycle.
//
// In the case it *may* be part of a cycle, we create a Weak<T> reference to the
// internal Arc<T> item, and store that Weak reference on the "defer list" of
// a collector object. We then occasionally scan all the items on the defer list
// from another thread via [Cycle Collection], and if we see that the objects
// are *definitely* unreachable from mutator thread still, we break cyclic
// references and cause the items to be deallocated.
//
// If the object has a count of 1, we can do another optimization as well: we
// can inspect the `weak_count` of the item, and if its >0 then it has a currently
// alive Weak<T> reference, which is almost definitely a reference held by the
// defer list. We optimistically try to *remove* the item from the defer list then,
// in the hopes that we are able to remove the Weak reference before the collector
// needs to scan and tell its dead itself, and allow the item to be fully deallocated
// sooner.

// Note [Cycle Collection]
// When doing a collection, we visit every item reachable from the roots,
// initializing them in a scratch space with an initial value of `strong_count`
// and decrementing the value every time we see a strong reference to the item,
// along with building a graph of edges for reachability.
// If, after we're done visiting the roots, we have a scratch space of items
// forming a strongly connected component, all with a value=0, then we know that
// all of the strong references to an item are internal to our visited graph;
// this means that it is a leaked cycle of objects, because if there were any
// live references from a mutator into the graph it would have a strong reference
// we weren't able to find.

impl PartialEq for Box<dyn WeakGc + 'static> {
    fn eq(&self, rhs: &Self) -> bool {
        self.as_ptr() == rhs.as_ptr()
    }
}

impl Eq for Box<dyn WeakGc + 'static> {
}

impl core::hash::Hash for Box<dyn WeakGc + 'static> {
    fn hash<H>(&self, hasher: &mut H) where H: core::hash::Hasher {
        hasher.write_usize(self.as_ptr())
    }
}

impl Collector {
    fn collect(&mut self) {
        // Create a local scratchpad for visited GC items
        let visited = HashSet::<usize>::new();
        let mut deferred = Default::default();
        mem::swap(&mut self.deferred, &mut deferred);
        // Visit all the possibly-unreachable cycle roots
        let mut queue = deferred.iter().collect::<Vec<_>>();
        while let Some(item) = queue.pop() {
            self.add_node(&**item);
            println!("collection visiting 0x{:x}", item.as_ptr());
            let x = item.visit(self);
        }
        mem::swap(&mut self.deferred, &mut deferred);
        // Now we break cycles from our roots using the [Cycle Collection]
        // scheme while taking care to handle [Graph Tearing]
        for (node, (count, incoming)) in self.visited.iter() {
            println!("{:x}:{}:{}", node, count, incoming);
        }
        self.visited = Default::default();
    }

    pub fn add_node(&mut self, root: &dyn WeakGc) {
        println!("got new node {:x}", root.as_ptr());
        self.visited.entry(root.as_ptr()).or_insert_with(|| (root.strong_count(), 0) );
        self.deferred.insert(root.root());
    }

    pub fn add_edge(&mut self, root: &dyn WeakGc, item: &dyn WeakGc) {
        println!("got edge {:x}->{:x}", root.as_ptr(), item.as_ptr());
        self.visited.entry(root.as_ptr()).and_modify(|e| e.1 += 1 ).or_insert_with(|| panic!() );
    }

    fn process(&mut self) {
        let chan = COLLECTOR.1.try_lock().expect("multiple collectors were started?");
        while let Ok(soul) = chan.recv() {
            println!("collector received soul");
            match soul {
                Soul::Died(d) => {
                    match d.as_ptr() {
                        // We could've been given a Weak<T> that has since been dropped
                        0 => (),
                        ptr => {
                            println!("got possibly-cyclic object 0x{:x}", ptr);
                            self.deferred.insert(d);
                        }
                    }
                },
                Soul::Reclaimed(d) => {
                    println!("told about a definitely not cyclic object 0x{:x}", d);

                },
                Soul::Yuga(b) => {
                    println!("triggering yuga");
                    self.collect();
                    // signal to all listeners that the yuga ended
                    *b.0.lock().unwrap() = true;
                    b.1.notify_all();
                }
                _ => {}
            }
        }
        println!("collector exiting");
    }

    /// Start the collector thread.
    fn start() -> JoinHandle<()> {
        std::thread::spawn(|| {
            let mut collector: Self = Default::default();
            collector.process();
        })
    }

    /// Trigger a cycle collection.
    pub fn yuga() {
        let b = Arc::new((Mutex::new(false), Condvar::new()));
        LOCAL_SENDER.with(|l| l.send(Soul::Yuga(b.clone())) ).unwrap();

        // wait for the yuga to end
        let mut lock = b.0.lock().unwrap();
        while !*lock { lock = b.1.wait(lock).unwrap(); }
        println!("collection done");
    }
}

/// As GC objects are dropped, threads send Souls to the global Collector via
/// channel; the collector thread then processes the souls, and occasionally
/// triggers cycle detection.
pub enum Soul {
    /// A possibly-cyclic object died, and must be added to the defer list for
    /// cycle checking later.
    Died(Box<dyn WeakGc>),
    /// An object died, and it definitely wasn't cyclic, but may have been added
    /// to the defer list and should be removed if so.
    Reclaimed(usize /* *const () */),
    /// A main thread wants to run a cycle collection immediately. Mostly used
    /// for testing.
    Yuga(Arc<(Mutex<bool>, Condvar)>),
}

lazy_static::lazy_static! {
    // TODO: see if it's better to do a concurrent hashset instead of serializing
    // over a channel; our defer set is actually the dual of a lattice, since if
    // a thread is removing an item from the set it can never be concurrently
    // being added by another, and if two threads are adding an item to the defer
    // list we can just drop one of them when we join the lattice.
    pub static ref COLLECTOR: (Mutex<SyncSender<Soul>>, Mutex<Receiver<Soul>>) = {
        let (send, recv) = sync_channel(128);
        (Mutex::new(send), Mutex::new(recv))
    };
}

lazy_static::lazy_static! {
    pub static ref COLLECTOR_HANDLE: Arc<RwLock<JoinHandle<()>>> = {
        Arc::new(RwLock::new(Collector::start()))
    };
}

thread_local! {
    pub static LOCAL_SENDER: SyncSender<Soul> = {
        let send = COLLECTOR.0.lock().unwrap().clone();
        COLLECTOR_HANDLE.read(); // touch the collector handle so that it starts if needed
        send
    }
}
