use crate::trace::Trace;
use crate::gc::{Gc, WeakGc};

use std::cell::RefCell;
use std::sync::{Arc, Weak, Mutex, RwLock, Barrier};
// TODO: see if crossbeam mpmc is faster than std mpsc?
use std::sync::mpsc::{Sender, Receiver, sync_channel, SyncSender};
use std::thread::JoinHandle;

use im::HashSet;

#[derive(Default)]
pub struct Collector {
    deferred: HashSet<Box<dyn WeakGc + 'static>>,
}

impl Collector {
    fn collect(&mut self) {
        // Snapshot the current set of possible cycle roots
        let snapshot = self.deferred.clone();
        // Create a local scratchpad for visited GC items
    }

    pub fn visit(&mut self, item: &dyn Trace) {
    }

    fn process(&mut self) {
        let chan = COLLECTOR.1.try_lock().expect("multiple collectors were started?");
        while let Ok(soul) = chan.recv() {
            println!("collector received soul");
            match soul {
                Soul::Died(d) => {
                    match d.as_ptr() {
                        // We could've been given a Weak<T> that has since been dropped
                        None => (),
                        Some(ptr) => {
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
                    b.wait();
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
        let b = Arc::new(Barrier::new(2));
        LOCAL_SENDER.with(|l| l.send(Soul::Yuga(b.clone())) ).unwrap();
        b.wait();
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
    Yuga(Arc<Barrier>),
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
