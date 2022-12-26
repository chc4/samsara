use crate::trace::Trace;
use crate::gc::{Gc, WeakRoot, GcFlags};
use crate::tracker::{Tracker, TrackerLocation};

use std::cell::{RefCell, Cell};
use std::mem;
use core::marker::PhantomData;
use std::collections::VecDeque;

use im::{HashSet, HashMap, OrdSet, OrdMap};
use roaring::RoaringBitmap;

#[cfg(all(feature = "shuttle", test))]
use shuttle::thread_local;

#[cfg(not(all(feature = "shuttle", test)))]
use std::{sync::{Arc, Weak, Mutex, RwLock, RwLockReadGuard, Condvar, atomic::{AtomicUsize, AtomicBool, Ordering}}, thread};
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
#[derive(Copy, Clone, Hash, PartialEq, Eq)]
pub struct NodeId(u32);

impl<'a> Clone for Root<'a> {
    fn clone(&self) -> Self {
        Root(WeakRoot(self.0.0.clone()), PhantomData)
    }
}

#[derive(Default)]
pub struct Collector<'a> {
    /// deferred: ptr -> Root
    deferred: OrdMap<*const (), Root<'a>>,
    /// visited: ptr -> (starting strong count, id, incoming edge count)
    pub visited: HashMap<*const (), (NodeId, usize)>,
    /// neighbors: // adjacency list for visited ids. tracks *outgoing* edges.
    neighbors: Vec<(*const (), RoaringBitmap)>,
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
// TODO: this is wrong, fix it
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

#[derive(Clone, Copy)]
enum GraphNode {
    Unknown,
    Dead,
    Live,
    Dirty,
    Reachable,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Copy)]
enum Component {
    Unset(usize),
    Root(usize),
    Live,
}

impl<'a> Collector<'a> {
    fn collect(&mut self) {
        println!("ENTER COLLECT");
        let mut roots = OrdMap::new();
        // save the initial root set of nodes we have to visit
        // also clear the deferred list, so that the OrdMap::diff later finds
        // the roots as edges when visiting the graph.
        mem::swap(&mut roots, &mut self.deferred);

        let mut to_visit: VecDeque<(*const (), Root)> = VecDeque::new();
        let mut alive = HashSet::<*const ()>::new();
        for root in roots.clone() {
            to_visit.push_front(root);
            while let Some((ptr, item)) = to_visit.pop_front() {
                println!("visit start for item {:?}", ptr);
                // skip the item if we already visited it or it's been deallocated
                if alive.contains(&ptr) {
                    println!("skipping already known unvisitable node {:?}", ptr);
                    continue;
                }
                let visited = item.0.flags().map(|flag| flag.contains(GcFlags::VISITED));
                if visited == None {
                    // we couldn't update the weak reference, mark it so we drop
                    // the weak reference in the eager alive dropping phase
                    println!("skipping dead root {:?}", ptr);
                    alive.insert(ptr);
                    continue;
                }
                if visited == Some(true) {
                    println!("skipping frontier item {:?}", ptr);
                    continue;
                }
                // snapshot of all the known nodes
                let snap = self.deferred.clone();
                println!("visiting {:?}", item.0.as_ptr());
                self.add_node(&item.0);
                item.0.mark_visited();
                let visited = item.0.flags();
                assert_ne!(visited, Some(GcFlags::NONE));
                let visitable = item.0.visit(self);
                if !visitable {
                    println!("figure out how to shortcut");
                    // XXX: is this good?
                    item.0.clear_visited();
                    alive.insert(ptr);
                    continue;
                }
                assert!(self.visited.contains_key(&ptr));
                // get the newly found nodes from the frontier, and push them
                // to the to_visit stack
                let items: Vec<(*const (), Root)> = snap.diff(&self.deferred).map(|item| match item {
                    im::ordmap::DiffItem::Add(k, item) => (*k, item.clone()),
                    im::ordmap::DiffItem::Update { old, new } => (*new.0, new.1.clone()),
                    im::ordmap::DiffItem::Remove(_o, _i) => panic!(),
                }).collect();
                to_visit.extend(items);
            }
        }
        let binding = self.visited.clone();
        let pre_nodes = binding.iter().collect();
        let mut graph_nodes = Vec::from_iter((0..self.visited.len()).map(|_| GraphNode::Unknown));
        // (from, to) node ids
        let graph_edges = Vec::from_iter(
            self.neighbors.iter().enumerate().flat_map(|(id, edges)|
                edges.1.iter().map(move |dst| (id, dst)) ));
        // map the alive ptrs to alive node ids
        let mut alive_ids: HashSet<NodeId> = alive.iter().filter_map(|ptr| {
            // and eagerly drop the weak references to all known live objects, so that
            // they can be freed if we were the only ones keeping them alive.
            self.deferred.remove(&ptr);
            let Some(id) = self.visited.get(ptr).map(|e| e.0) else {
                // the object wasn't in visited list, it was a dead pointer
                self.visited.remove(&ptr);
                return None };
            graph_nodes[id.0 as usize] = GraphNode::Live;
            self.visited.remove(&ptr);
            Some(id)
        }).collect();
        drop(alive);
        // make a visitation set of node ids
        let mut to_visit_ids = VecDeque::<NodeId>::new();

        // find all nodes that don't have a strong_count == incoming_edge_count.
        // the unaccounted for edge must be from the mutator, and thus the node
        // is live. when we find live objects we also do a traversal from it in
        // order to mark all objects reachable from it as also live, since they
        // are transitively reachable from the mutator as well.
        'visit: for (ptr, row) in &self.visited {
            println!("'visit {:?}", ptr);
            // if the node is already marked alive, we're done with it
            if alive_ids.contains(&row.0) {
                continue;
            }
            let Some(node) = self.deferred.get(&ptr) else { panic!() };
            let Some(flags) = node.0.flags() else { panic!("what do we do here?") };
            if !flags.contains(GcFlags::VISITED) { panic!("can this happen? {:?}", flags) }
            'check_live: {
                // we have to be careful about the order here - we check the flags
                // after we get the count because a mutator might be losing a
                // reference at the same time we are trying to check it
                let count = Weak::strong_count(&node.0.0);
                if flags.contains(GcFlags::DIRTY) {
                    // alive because mutator set flag
                    graph_nodes[row.0.0 as usize] = GraphNode::Dirty;
                    break 'check_live;
                }
                if count == row.1 {
                    graph_nodes[row.0.0 as usize] = GraphNode::Dead;
                    continue 'visit;
                } else {
                    // alive because we have external edge from mutator
                    graph_nodes[row.0.0 as usize] = GraphNode::Live;
                    break 'check_live;
                }
            };
            println!("found new alive root node {:?}", ptr);
            alive_ids.insert(row.0);
            to_visit_ids.push_front(row.0);
            while let Some(outgoing) = to_visit_ids.pop_back() {
                // sanity check that we're always pushing forward only the alive node frontier
                #[cfg(test)]
                assert!(alive_ids.contains(&outgoing));
                let outgoing_edges = &self.neighbors[outgoing.0 as usize];
                for outgoing_neighbor in &outgoing_edges.1 {
                    if alive_ids.contains(&NodeId(outgoing_neighbor)) {
                        continue;
                    }
                    graph_nodes[outgoing_neighbor as usize] = GraphNode::Reachable;
                    alive_ids.insert(NodeId(outgoing_neighbor));
                    to_visit_ids.push_front(NodeId(outgoing_neighbor));
                }
                //TODO: we can free the weak_ref for alive visited nodes here too probably
            }
        }

        #[cfg(feature = "graphviz")]
        self.draw_graph(pre_nodes, graph_nodes, graph_edges);

        // now destroy all unreachable dead nodes
        for (ptr, row) in &self.visited {
            let node = self.deferred.get(&ptr).unwrap();
            node.0.clear_visited();

            if !alive_ids.contains(&row.0) {
                println!("got dead node {:?}", ptr);
                node.0.invalidate();
            }
        }

        #[cfg(test)]
        for node in self.deferred.iter() {
            // in test mode, verify that we cleared all object flags
            assert!(node.1.0.flags().map(|f| f != GcFlags::NONE) != Some(true),
                "still marked node #{} {:?}", self.visited[node.0].0.0, node.0);
        }
        println!("done");

        // We can now reset our working sets of objects; possibly-cyclic data
        // from the worker threads will all be added later since it's buffered
        // in the channel while we were working.
        self.visited = Default::default();
        self.deferred = Default::default();
        self.neighbors = Default::default();
    }

    #[cfg(feature = "graphviz")]
    fn draw_graph(&self, pre_nodes: Vec<(&*const (), &(NodeId, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, u32)>) {
        use graphviz_rust::*;
        use graphviz_rust::printer::*;
        use graphviz_rust::cmd::*;
        use dot_structures::*;
        use dot_generator::*;
        let g = graph!(strict di id!("t"); subgraph!("s",
            pre_nodes.iter().flat_map(|(ptr, (idx, incoming))| {
                let (_, edges) = &self.neighbors[idx.0 as usize];
                let mut edges = edges.iter().map(|e| {
                    Stmt::Edge(edge!(node_id!(e) => node_id!(idx.0)))
                }).collect::<Vec<Stmt>>();
                let color = match &after_nodes[idx.0 as usize] {
                    GraphNode::Unknown => "orange",
                    GraphNode::Dead => "grey",
                    GraphNode::Dirty => "black",
                    GraphNode::Live => "green",
                    GraphNode::Reachable => "lime",
                };
                let label = format!("\"{} {:?}:{}\"",
                    idx.0,
                    self.deferred.get(&ptr).map(|n| Weak::strong_count(&n.0.0)).unwrap_or(0), incoming);
                edges.push(Stmt::Node(node!(idx.0; attr!("color", color), attr!("label", label))));
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
    fn draw_graph(&self, pre_nodes: Vec<(&*const (), &(NodeId, usize))>, after_nodes: Vec<GraphNode>, after_edges: Vec<(usize, usize)>) {
        // noop
    }

    pub fn add_node(&mut self, root: &WeakRoot) -> bool {
        if root.as_ptr() as usize == 0 { panic!() }
        let mut novel = false;
        self.visited.entry(root.as_ptr()).or_insert_with(|| {
            // We didn't already visit the gc object
            println!("got new node {:?}", root.as_ptr());
            //root.mark_visited();
            self.deferred.entry(root.as_ptr()).or_insert_with(|| {
                Root(root.clone(), PhantomData)
            });
            let idx = self.neighbors.len();
            self.neighbors.push((root.as_ptr(), RoaringBitmap::new()));
            novel = true;
            (NodeId(idx as u32), 0)
        });
        // it's probably useful to be able to tell if we are first seeing a node
        // or not...? i can't think of for what right now, though
        novel
    }

    pub fn add_edge(&mut self, root: &WeakRoot, item: WeakRoot) {
        // XXX: this has absolutely terrible cache locality
        println!("got edge {:?}->{:?}", root.as_ptr(), item.as_ptr());
        let root_idx = self.visited.entry(root.as_ptr()).or_insert_with(|| panic!() ).0;
        let item_row = self.visited.entry(item.as_ptr()).or_insert_with(|| panic!() );
        // increment the item's incoming count
        item_row.1 += 1;
        // add outgoing edge to our graph
        self.neighbors[root_idx.0 as usize].1.insert(item_row.0.0);
    }

    fn process(&mut self, receiver: Receiver<Soul>) {
        while let Ok(soul) = { receiver.recv() }{
            println!("collector received soul");
            match soul {
                Soul::Died(d) => {
                    match d.as_ptr() as usize {
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
                    self.deferred.remove(&(d as *const ()));
                },
                Soul::Yuga(b) => {
                    println!("triggering yuga");
                    self.collect();
                    // signal to all listeners that the yuga ended
                    *b.0.lock().unwrap() = true;
                    b.1.notify_all();
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
