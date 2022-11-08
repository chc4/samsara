use crate::trace::Trace;
use crate::gc::{Gc, WeakGc, GcFlags};

use std::cell::RefCell;
use std::mem;
use std::rc::Rc;
use std::sync::{Arc, Weak, Mutex, RwLock, Condvar};
// TODO: see if crossbeam mpmc is faster than std mpsc?
use std::sync::mpsc::{Sender, Receiver, sync_channel, SyncSender};
use std::thread::JoinHandle;
use core::marker::PhantomData;

use im::{HashSet, HashMap, OrdSet, OrdMap};
use roaring::RoaringBitmap;
use reunion::{UnionFind, UnionFindTrait};

struct Root<'a>(Rc<dyn WeakGc>, PhantomData<&'a ()>);

impl<'a> Clone for Root<'a> {
    fn clone(&self) -> Self {
        Root(self.0.clone(), PhantomData)
    }
}

#[derive(Default)]
pub struct Collector<'a> {
    deferred: OrdMap<usize, Root<'a>>,
    pub visited: HashMap<usize, (usize, usize, usize)>, // ptr -> (starting strong count, incoming edges, id)
    neighbors: Vec<RoaringBitmap>,
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

impl<'a> PartialEq for Root<'a> {
    fn eq(&self, rhs: &Self) -> bool {
        self.0.as_ptr() == rhs.0.as_ptr()
    }
}

impl<'a> Eq for Root<'a> {
}

impl<'a> core::hash::Hash for Root<'a> {
    fn hash<H>(&self, hasher: &mut H) where H: core::hash::Hasher {
        hasher.write_usize(self.0.as_ptr())
    }
}

impl<'a> PartialOrd for Root<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.as_ptr().partial_cmp(&self.0.as_ptr())
    }
}

impl<'a> Ord for Root<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_ptr().cmp(&self.0.as_ptr())
    }
}

impl<'a> Collector<'a> {
    fn collect(&mut self) {
        // Create a local scratchpad for visited GC items
        let mut visited = OrdMap::new();
        // Visit all the possibly-unreachable cycle roots
        loop {
            // Visit all the items in the list that we haven't already visited
            let snap = self.deferred.clone();
            for item in visited.diff(&snap) {
                let item = match item {
                    im::ordmap::DiffItem::Add(_, item) => item,
                    im::ordmap::DiffItem::Update { old, new } => new.1,
                    im::ordmap::DiffItem::Remove(o, i) => { println!("REMOVED"); i },
                };
                self.add_node(&*item.0);
                println!("collection visiting 0x{:x}", item.0.as_ptr());
                let visitable = item.0.visit(self);
                if !visitable {
                    println!("figure out how to shortcut");
                    // probably just clear_visited() on it and then
                    // remove it from both the visited and snapshot sets?
                    // we don't know anything about gc objects reachable from this
                    // live objects, so we can't 
                }
            }
            visited = snap;
            if self.deferred.ptr_eq(&visited) {
                // we didn't add any new items, and we're done
                break
            }
        }
        // We've visited all the nodes in our graph! The only possible candidates
        // for cycles are nodes where their initial count == incoming AND they aren't
        // marked DIRTY.
        let mut nodes = self.visited.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|n| n.1.2);
        let mut components = UnionFind::<usize>::new();
        for (node, (count, incoming, idx)) in nodes {
            let gc = self.deferred.get(node).expect(format!("{:x}", node).as_str());
            let edges = &self.neighbors[*idx];
            let flags = gc.0.flags().unwrap();
            println!("{:x}:{}:{} ({:?}) - {:?}", node, count, incoming,
                flags, edges);
            gc.0.clear_visited();
            if count == incoming && !flags.contains(GcFlags::DIRTY) {
                for edge in edges {
                    if components.find(*idx) == components.find(edge as usize) {
                        println!("part of cycle");
                        gc.0.invalidate();
                        break; // ???
                    }
                    components.union(*idx, edge as usize);
                }
            }
        }
        // TODO cycle break
        // We can now reset our working sets of objects; possibly-cyclic data
        // from the worker threads will all be added against since it's buffered
        // in the channel while we were working.
        self.visited = Default::default();
        self.deferred = Default::default();
        self.neighbors = Default::default();
    }

    pub fn add_node(&mut self, root: &dyn WeakGc) -> bool {
        if root.as_ptr() == 0 { panic!() }
        let mut novel = false;
        self.visited.entry(root.as_ptr()).or_insert_with(|| {
            // We didn't already visit the gc object, so we are initially adding
            // it to the graph; we set the VISIT flag for [Graph Tearing] and then
            // initialize the vertex with the correct weight, and also add it to
            // our list of all root nodes so that we visit it later.
            println!("got new node {:x}", root.as_ptr());
            root.mark_visited();
            self.deferred.entry(root.as_ptr()).or_insert_with(|| {
                Root(root.realloc(), PhantomData)
            });
            let idx = self.neighbors.len();
            self.neighbors.push(RoaringBitmap::new());
            novel = true;
            (root.strong_count(), 0, idx)
        });
        // it's probably useful to be able to tell if we are first seeing a node
        // or not...? i can't think of for what right now, though
        novel
    }

    pub fn add_edge(&mut self, root: &dyn WeakGc, item: &dyn WeakGc) {
        println!("got edge {:x}->{:x}", root.as_ptr(), item.as_ptr());
        let root_idx = self.visited.entry(root.as_ptr()).and_modify(|e| {
            // TODO: do we need incoming edge count now that we have adjacency lists?
            e.1 += 1
        }).or_insert_with(|| panic!() ).2;
        // add the reverse edge to our graph
        self.neighbors[self.visited.get(&item.as_ptr()).unwrap().2].insert(root_idx as u32);
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
                            self.deferred.entry(d.as_ptr()).or_insert_with(|| {
                                println!("inserting");
                                Root(d.realloc(), PhantomData)
                            });
                        }
                    }
                },
                Soul::Reclaimed(d) => {
                    println!("told about a definitely not cyclic object 0x{:x}", d);
                    self.deferred.remove(&d);
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
