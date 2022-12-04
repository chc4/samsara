use crate::trace::Trace;
use crate::gc::{Gc, WeakRoot, GcFlags};
use crate::tracker::{Tracker, TrackerLocation};

use std::cell::{RefCell, Cell};
use std::rc::Rc;
use std::mem;
use core::marker::PhantomData;
use std::collections::VecDeque;

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
    pub visited: HashMap<usize, (usize, usize)>, // ptr -> (starting strong count, id)
    neighbors: Vec<(usize, RoaringBitmap)>, // adjacency list for visited ids. tracks *incoming* edges.
    pub stack: VecDeque<usize>, // stack of ids
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

#[derive(Eq, PartialEq, Hash, Clone, Debug, Copy)]
enum Component {
    Unset(usize),
    Root(usize),
    Live,
}

impl<'a> Collector<'a> {
    fn collect(&mut self) {
        let mut roots = OrdMap::new();
        // save the initial root set of nodes we have to visit
        // also clear the deferred list, so that the OrdMap::diff later finds
        // the roots as edges when visiting the graph.
        mem::swap(&mut roots, &mut self.deferred);

        // for each root in our root set, we do a full preorder BFS traversal of
        // all its children; this also builds up a stack of nodes in postorder
        // to use in the second pass.
        // this is kinda weird, but we need to make sure we do a full BFS instead
        // of visiting the entire root set, then the entire next frontier, etc.
        // because we need to maintain the "parents are visited before children"
        // invariant, which it would break - we could have two root ones, with
        // the second one a child of the first.
        let mut to_visit: VecDeque<(usize, Root)> = roots.clone().into_iter().collect();
        while let Some((ptr, item)) = to_visit.pop_back() {
            // skip the item if we already visited it or it's been deallocated
            if item.0.flags().map(|flag| flag.contains(GcFlags::VISITED)) != Some(false) {
                println!("skipping root {:x}", ptr);
                continue;
            }
            // snapshot of all the known nodes
            let snap = self.deferred.clone();
            println!("visiting {:x}", item.0.as_ptr());
            self.add_node(&item.0);
            item.0.mark_visited();
            let visitable = item.0.visit(self);
            if !visitable {
                println!("figure out how to shortcut");
                //item.0.clear_visited();
                // can we just clear_visited? does this break if you have one root's
                // traversal be unable to visit the node because it is locked, and
                // then another's traveral later be able to visit it, does it
                // maintain our postorder invariant?
            }
            // get the newly found nodes from the frontier, and push them
            // to the to_visit stack
            let items: Vec<(usize, Root)> = snap.diff(&self.deferred).map(|item| match item {
                im::ordmap::DiffItem::Add(k, item) => (*k, item.clone()),
                im::ordmap::DiffItem::Update { old, new } => (*new.0, new.1.clone()),
                im::ordmap::DiffItem::Remove(_o, _i) => panic!(),
            }).collect();
            if items.len() == 0 {
                self.stack.push_back(ptr);
            }
            to_visit.extend(items);
        }

        // We've visited all the nodes in our graph! The only possible candidates
        // for cycles are nodes where their initial count == incoming AND they aren't
        // marked DIRTY.
        let mut nodes = self.visited.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|n| n.1.1);
        let pre_nodes = nodes.clone();

        let mut components = UnionFind::<Component>::new();
        let mut graph_nodes = vec![];
        let mut graph_edges = vec![]; // (from, to)
        //for (node, (count, idx)) in nodes.drain(..) {
        while let Some(node) = self.stack.pop_back() {
            let (ref count, ref idx) = self.visited[&node];
            let gc = self.deferred.get(&node).expect(format!("{:x}", node).as_str());
            let Some(flags) = gc.0.flags() else { graph_nodes.push(GraphNode::Dead); continue };
            if !flags.contains(GcFlags::VISITED) { continue; }
            let edges = &self.neighbors[*idx];
            let incoming = edges.1.len();
            println!("#{} {:x}:{}:{} ({:?}) - {:?}", idx, node, count, incoming,
                flags, edges.1);
            // we have to reset the bitflags for all of our objects, in case they
            // *weren't* dirty.
            gc.0.clear_visited();
            if flags.contains(GcFlags::DIRTY) {
                // mutator lost and possibly gained a reference, have to assume it's alive
                components.union(Component::Unset(*idx), Component::Live);
                graph_nodes.push(GraphNode::Dirty);
            }
            else if *count as u64 == incoming {
                // count is correct! assign to a component and visit in-neighbors
                graph_nodes.push(GraphNode::Internal);
            } else {
                // we could start tearing down the graph here i guess
                graph_nodes.push(GraphNode::Live);
                components.union(Component::Unset(*idx), Component::Live);
            }
            for edge in &edges.1 {
                graph_edges.push((edge as usize, *idx));
            }
            let comp = components.find(Component::Unset(*idx));
            if comp == Component::Unset(*idx) {
                components.union(comp, Component::Root(*idx));
            }
            for edge in &edges.1 {
                let identity = Component::Unset(edge as usize);
                let edge_comp = components.find(identity);
                println!("incoming edge {} of {:?}", edge, edge_comp);
                if edge_comp == identity {
                    self.stack.push_back(self.neighbors[edge as usize].0 as usize);
                    components.union(comp, identity);
                }
            }
        }
        assert_eq!(pre_nodes.len(), graph_nodes.len());
        for component in components.subsets() {
            println!("component {:?} {}", component, component.contains(&Component::Live));
            if !component.contains(&Component::Live) {
                for member in component.iter().filter_map(|i| if let Component::Unset(i) = i { Some(i) } else { None }) {
                    graph_nodes[*member] = GraphNode::Cyclic;
                    println!("invalidating {}", member);
                    println!("{:?}", self.deferred.keys().collect::<Vec<_>>());
                    let gc = &self.deferred[pre_nodes[*member].0];
                    gc.0.invalidate();
                }
                //self.draw_graph(pre_nodes, graph_nodes, graph_edges);
            }
        }
        self.draw_graph(pre_nodes, graph_nodes, graph_edges, &mut components);

        // We can now reset our working sets of objects; possibly-cyclic data
        // from the worker threads will all be added later since it's buffered
        // in the channel while we were working.
        self.visited = Default::default();
        self.deferred = Default::default();
        self.neighbors = Default::default();
        self.stack = Default::default();
    }

    #[cfg(feature = "graphviz")]
    fn draw_graph(&self, pre_nodes: Vec<(&usize, &(usize, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, usize)>, components: &mut UnionFind<Component>) {
        use graphviz_rust::*;
        use graphviz_rust::printer::*;
        use graphviz_rust::cmd::*;
        use dot_structures::*;
        use dot_generator::*;
        let g = graph!(strict di id!("t"),
          components.subsets().iter().enumerate().map(|(i, sub)| { Stmt::Subgraph(
            subgraph!(format!("cluster{}",i),
            {let mut sub = sub.iter().flat_map(|node| {
                let Component::Unset(node) = node else { return vec![] };
                let (node, (count, idx)) = pre_nodes[*node];
                let (_, edges) = &self.neighbors[*idx];
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
            }).collect::<Vec<_>>();
            sub.push(Stmt::Attribute(attr!("color", "blue")));
            sub.push(Stmt::Attribute(attr!("label", i)));
            sub}
          )) }).collect()
        );

        let graph_svg = exec(g, &mut PrinterContext::default(), vec![
            CommandArg::Format(Format::Pdf),
            CommandArg::Output("./samsara_graph.pdf".to_string())
        ]).unwrap();
        println!("outputted graph {}", graph_svg);
    }

    #[cfg(not(feature = "graphviz"))]
    fn draw_graph(&self, pre_nodes: Vec<(&usize, &(usize, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, usize)>, components: &mut UnionFind<Component>) {
        // noop
    }

    pub fn add_node(&mut self, root: &WeakRoot) -> bool {
        if root.as_ptr() == 0 { panic!() }
        let mut novel = false;
        self.visited.entry(root.as_ptr()).or_insert_with(|| {
            // We didn't already visit the gc object, so we are initially adding
            // it to the graph; we set the VISIT flag for [Graph Tearing] and then
            // initialize the vertex with the correct weight, and also add it to
            // our list of all root nodes so that we visit it later.
            println!("got new node {:x}", root.as_ptr());
            //root.mark_visited();
            self.deferred.entry(root.as_ptr()).or_insert_with(|| {
                Root(root.clone(), PhantomData)
            });
            let idx = self.neighbors.len();
            self.neighbors.push((root.as_ptr(), RoaringBitmap::new()));
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
        self.neighbors[self.visited[&item.as_ptr()].1].1.insert(root_idx as u32);
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
