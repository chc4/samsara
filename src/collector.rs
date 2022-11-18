use crate::trace::Trace;
use crate::gc::{Gc, WeakRoot, GcFlags};
use crate::tracker::{Tracker, TrackerLocation};

use std::cell::{RefCell, Cell};
use std::rc::Rc;
use std::mem;
use core::marker::PhantomData;

use im::{HashSet, HashMap, OrdSet, OrdMap};
use roaring::RoaringBitmap;
use reunion::{UnionFind, UnionFindTrait};

#[cfg(all(feature = "shuttle", test))]
use shuttle::thread_local;

#[cfg(not(all(feature = "shuttle", test)))]
use std::{sync::{Once, Arc, Weak, Mutex, RwLock, RwLockReadGuard, Condvar, atomic::{AtomicUsize, AtomicBool, Ordering}}, thread};
#[cfg(not(all(feature = "shuttle", test)))]
use rand;

#[cfg(all(feature = "shuttle", test))]
use shuttle::{sync::{*, atomic::{AtomicUsize, AtomicBool, Ordering}}, thread, rand};

// TODO: see if crossbeam mpmc is faster than std mpsc?
#[cfg(not(all(feature = "shuttle", test)))]
pub use std::sync::mpsc::{Sender, Receiver, sync_channel, SyncSender};
#[cfg(all(feature = "shuttle", test))]
pub use shuttle::sync::mpsc::{Sender, Receiver, sync_channel, SyncSender};

struct Root<'a>(WeakRoot, PhantomData<&'a ()>);

impl<'a> Clone for Root<'a> {
    fn clone(&self) -> Self {
        Root(WeakRoot(self.0.0.clone()), PhantomData)
    }
}

#[derive(Default)]
pub struct Collector<'a> {
    deferred: OrdMap<usize, Root<'a>>,
    pub visited: HashMap<usize, (usize, usize)>, // ptr -> (starting strong count, incoming edges, id)
    neighbors: Vec<RoaringBitmap>, // adjacency list for visited ids. tracks *incoming* edges.
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
        self.0.0.as_ptr() == rhs.0.0.as_ptr()
    }
}

impl<'a> Eq for Root<'a> {
}

impl<'a> core::hash::Hash for Root<'a> {
    fn hash<H>(&self, hasher: &mut H) where H: core::hash::Hasher {
        hasher.write_usize(self.0.0.as_ptr() as *const () as usize)
    }
}

impl<'a> PartialOrd for Root<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.0.as_ptr().partial_cmp(&self.0.0.as_ptr())
    }
}

impl<'a> Ord for Root<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.0.as_ptr().cmp(&self.0.0.as_ptr())
    }
}

enum GraphNode {
    Unknown,
    Dead,
    Live,
    Dirty,
    Internal,
    Cyclic
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
                println!("-- collection visiting 0x{:x}", item.0.as_ptr());
                self.add_node(item.0.clone());
                let visitable = item.0.visit(self);
                if !visitable {
                    println!("figure out how to shortcut");
                    item.0.clear_visited();
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
        nodes.sort_by_key(|n| n.1.1);
        let pre_nodes = nodes.clone();

        let mut components = UnionFind::<Option<usize>>::new();
        let mut graph_nodes = vec![];
        let mut graph_edges = vec![]; // (from, to)
        for (node, (count, idx)) in nodes.drain(..) {
            let gc = self.deferred.get(node).expect(format!("{:x}", node).as_str());
            let edges = &self.neighbors[*idx];
            let Some(flags) = gc.0.flags() else { graph_nodes.push(GraphNode::Dead); continue };
            let incoming = edges.len();
            println!("{:x}:{}:{} ({:?}) - {:?}", node, count, incoming,
                flags, edges);
            // we have to reset the bitflags for all of our objects, in case they
            // *weren't* dirty.
            gc.0.clear_visited();
            if flags.contains(GcFlags::DIRTY) {
                components.union(Some(*idx), None);
                graph_nodes.push(GraphNode::Dirty);
            }
            else if *count as u64 == incoming {
                graph_nodes.push(GraphNode::Internal);
                for edge in edges {
                    graph_edges.push((edge as usize, *idx));
                }
                for edge in edges {
                    // XXX: do we also have to check if edge is fine?
                    // if we are a part of the same component as an incoming
                    // edge, then we've found a cycle and break it.
                    let comp = components.find(Some(*idx));
                    let edge_comp = components.find(Some(edge as usize));
                    if edge_comp.is_none() {
                        // if we're downstream from a node we know isn't in a cycle,
                        // we should mark ourselves not part of a cycle also.
                        components.union(Some(*idx), None);
                        break;
                    }
                    else if comp.is_some() && comp == edge_comp {
                        println!("possibly part of cycle");
                        components.union(Some(*idx), Some(edge as usize));
                        break;
                    }
                    components.union(Some(*idx), Some(edge as usize));
                }
            } else {
                // we could start tearing down the graph here i guess
                graph_nodes.push(GraphNode::Live);
                components.union(Some(*idx), None);
            }
        }
        assert_eq!(pre_nodes.len(), graph_nodes.len());
        for component in components.subsets() {
            if !component.contains(&None) {
                let mut i = component.iter();
                while let Some(Some(member)) = i.next() {
                    graph_nodes[*member] = GraphNode::Cyclic;
                    println!("invalidating {}", member);
                    println!("{:?}", self.deferred.keys().collect::<Vec<_>>());
                    let gc = &self.deferred[pre_nodes[*member].0];
                    gc.0.invalidate();
                }
                //self.draw_graph(pre_nodes, graph_nodes, graph_edges);
                //panic!("component {:?}", component);
            }
        }

        self.draw_graph(pre_nodes, graph_nodes, graph_edges);
        // We can now reset our working sets of objects; possibly-cyclic data
        // from the worker threads will all be added later since it's buffered
        // in the channel while we were working.
        self.visited = Default::default();
        self.deferred = Default::default();
        self.neighbors = Default::default();
    }

    #[cfg(feature = "graphviz")]
    fn draw_graph(&self, pre_nodes: Vec<(&usize, &(usize, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, usize)>) {
        use graphviz_rust::*;
        use graphviz_rust::printer::*;
        use graphviz_rust::cmd::*;
        use dot_structures::*;
        use dot_generator::*;
        let g = graph!(strict di id!("t");
          subgraph!("x",
            pre_nodes.iter().flat_map(|(node, (count, idx))| {
                let edges = &self.neighbors[*idx];
                let mut edges = edges.iter().map(|e| {
                    Stmt::Edge(edge!(node_id!(e) => node_id!(idx)))
                }).collect::<Vec<Stmt>>();
                let color = match &after_nodes[*idx] {
                    GraphNode::Unknown => "white",
                    GraphNode::Dead => "grey",
                    GraphNode::Dirty => "black",
                    GraphNode::Live => "green",
                    GraphNode::Internal => "pink",
                    GraphNode::Cyclic => "red",
                };
                let label = format!("\"{} {}:{}\"",
                    idx,
                    //self.deferred.get(node).unwrap().0.as_ptr(),
                    count, edges.len());
                edges.push(Stmt::Node(node!(idx; attr!("color", color), attr!("label", label))));
                edges
            }).collect()
          )
        );

        let graph_svg = exec(g, &mut PrinterContext::default(), vec![
            CommandArg::Format(Format::Pdf),
            CommandArg::Output("./samsara_graph.pdf".to_string())
        ]).unwrap();
        println!("outputted graph {}", graph_svg);
    }

    #[cfg(not(feature = "graphviz"))]
    fn draw_graph(&self, pre_nodes: Vec<(&usize, &(usize, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, usize)>) {
        // noop
    }

    pub fn add_node(&mut self, root: WeakRoot) -> bool {
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
                Root(root.clone(), PhantomData)
            });
            let idx = self.neighbors.len();
            self.neighbors.push(RoaringBitmap::new());
            novel = true;
            let strong_count = root.0.strong_count();
            (strong_count, idx)
        });
        // it's probably useful to be able to tell if we are first seeing a node
        // or not...? i can't think of for what right now, though
        novel
    }

    pub fn add_edge(&mut self, root: &WeakRoot, item: WeakRoot) {
        println!("got edge {:x}->{:x}", root.as_ptr(), item.as_ptr());
        let root_idx = self.visited.entry(root.as_ptr()).or_insert_with(|| panic!() ).1;
        // add the reverse edge to our graph
        self.neighbors[self.visited[&item.as_ptr()].1].insert(root_idx as u32);
    }

    fn process(&mut self, receiver: Receiver<Soul>) {
        while let Ok(soul) = { receiver.recv() }{
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
                                Root(d, PhantomData)
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
                },
                Soul::Nirvana(()) => {
                    println!("thread achieved nirvana");
                },
                _ => {}
            }
        }
        println!("collector exiting");
    }

    /// Start the collector thread.
    fn start(recv: Receiver<Soul>) -> thread::JoinHandle<()> {
        #[cfg(all(feature = "shuttle", test))]
        let spawn = shuttle::thread::spawn;
        #[cfg(not(all(feature = "shuttle", test)))]
        let spawn = std::thread::spawn;

        spawn(|| {
            IS_COLLECTOR.with(|b| b.store(true, Ordering::Release));
            let mut collector: Self = Default::default();
            collector.process(recv);
        })
    }

    /// Trigger a cycle collection.
    pub fn yuga() {
        let b = Arc::new((Mutex::new(false), Condvar::new()));
        println!("mutator forcing collection");
        LOCAL_SENDER.with(|l| l.chan.borrow().as_ref().unwrap().send(Soul::Yuga(b.clone())) ).unwrap();

        // wait for the yuga to end
        let mut lock = b.0.lock().unwrap();
        while !*lock { lock = b.1.wait(lock).unwrap(); }
        println!("collection done");
    }

    //#[cfg(all(feature = "shuttle", test))]
    /// Randomly trigger a non-blocking collection
    pub fn maybe_yuga() {
        use self::rand::Rng;
        if self::rand::thread_rng().gen::<bool>() == false { return ; }
        let b = Arc::new((Mutex::new(false), Condvar::new()));
        println!("mutator forcing non-blocking collection");
        LOCAL_SENDER.with(|l| l.chan.borrow().as_ref().unwrap().send(Soul::Yuga(b.clone())) ).unwrap();

        // wait for the yuga to end
        println!("skipping wait");
    }

    #[cfg(all(feature = "shuttle", test))]
    /// Unfortunately, shuttle doesn't run thread-local destructors when threads
    /// *exit*, only when they are *joined*. This makes it report spurious dead-locks
    /// when the Collector calls recv() on the communications channel - under normal
    /// operations the LocalSender is dropped by all mutator threads and Weak<T> drops
    /// all live SyncSenders, causing recv() to unblock.
    pub fn nirvana() {
        LOCAL_SENDER.with(|l|{ l.chan.take(); });
    }

}

/// As GC objects are dropped, threads send Souls to the global Collector via
/// channel; the collector thread then processes the souls, and occasionally
/// triggers cycle detection.
pub enum Soul {
    /// A possibly-cyclic object died, and must be added to the defer list for
    /// cycle checking later.
    Died(WeakRoot),
    /// An object died, and it definitely wasn't cyclic, but may have been added
    /// to the defer list and should be removed if so.
    Reclaimed(usize /* *const () */),
    /// A main thread wants to run a cycle collection immediately. Mostly used
    /// for testing.
    Yuga(Arc<(Mutex<bool>, Condvar)>),
    /// A thread that had an open channel exitted.
    Nirvana(()),
}

thread_local! {
    pub static IS_COLLECTOR: AtomicBool = {
        AtomicBool::new(false)
    };
}

pub struct LocalSender {
    pub chan: RefCell<Option<Arc<SyncSender<Soul>>>>,
}

impl Drop for LocalSender {
    fn drop(&mut self) {
        println!("drop");
        let Self { chan } = self;
        //chan.borrow().as_ref().map(|chan| chan.send(Soul::Nirvana(())).unwrap());
        drop(chan);
    }
}

lazy_static::lazy_static! {
    static ref CHANNEL: RwLock<Weak<SyncSender<Soul>>> = {
        RwLock::new(Weak::new())
    };
}

thread_local! {
    pub static LOCAL_SENDER: LocalSender = {
        let mut guard = CHANNEL.write().unwrap();
        let send = if let Some(chan) = Weak::upgrade(&*guard) {
            // if there is already a channel for the collector, use it
            chan
        } else {
            // otherwise, we have to start a new collecotr
            let (send, recv) = sync_channel(128);
            let send = Arc::new(send);
            *guard = Arc::downgrade(&send);
            drop(guard);
            Collector::start(recv);
            send

        };
        LocalSender { chan: RefCell::new(Some(send)) }
    }
}
