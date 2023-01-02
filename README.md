# Samsara

Samsara is a Rust library that provides a `Gc<T>` reference-counted cycle collected smartpointer. You can use it as a mostly drop-in replacement for `Arc<RefCell<T>>`, except that calling `samsara::Collector::trigger()` will find and free any objects that are leaking memory due to cyclic references; this cycle collection is done by another thread concurrently with any mutators, and has no global stop-the-world phase.

Samsara is written in safe Rust, implemented over the `Arc<T>` type. Even if it incorrectly destroys objects that were in fact reachable (which it should not; if it does, report it as a bug please!) it will only cause panics via an `Option::unwrap`, not a use-after-free. It doesn't require any manual root object tracking like other garbage collector crates.

Drawbacks to Samsara are that dropping `Gc<T>` handles sends a message on a channel to the collector thread; this means that if you have a lot of mutators, they may start blocking on the collector draining the channel, which may take a while if it is currently doing a cycle collection. Additionally, the cycle collection may use an amount of memory proportional to the number of live objects that had been decremented since the last cycle collection; if you have a graph of 1M objects, and all of them are reachable from an object you cloned and then dropped but didn't free via the refcount going to zero, cycle collection will allocate a 1M entry hashmap and 1M adjacency list datastructure. Decremented objects that aren't destroyed will also be stored on the collector's list until a cycle collection starts, all; if you cloned and dropped all of the 1M live objects, the 1M entry hashmap will be populated with all the objects, using memory.

Cycle collection itself is currently *only* manually triggered, and doesn't run automatically. As a library user you need to call `Collector::trigger()` every once in a while.

Additionally, while Samsara doesn't have a *global* stop-the-world phase, it may temporarily cause mutator threads to block on acquiring a mutable reference to an object, if that object was in the process of being scanned by the collector thread; this should be short, with the lock only held for as long as it takes to visit that individual object and not the entire subgraph. Additionally, if the object has any interior mutable fields that aren't other `Gc<T>` handles, then acquiring *non-mut* reference to an object will also potentially block.

# Usage
`Gc<T>` has two APIs: `get` and `set` take an `Owner<T>` type as a parameter, which is a re-export of [qcell]'s `TCellOwner<T>` type. This allows for a static guarantee that access the `Gc<T>` potentially across different threads won't deadlock, since only one thread can have ownership of the owner singleton type at a time - you can optionally put the `Owner<T>` value in a normal `Arc<RefCell<T>>` to gain normal RefCell-like panic-on-double-get_mut semantics on acquiring the owner value.

It also provides `get_blocking` and `force_set` functions, which are the same but don't take an `Owner<T>` as a parameter. They have `Arc<RwLock<T>>` semantics, where attempting to `force_set` on an object with outstanding `get` or `get_blocking` calls may block or cause a deadlock, and vice-versa.

Additionally, there is `Gc<T>::set_multiple` and `Gc<T>::force_set_multiple` which take an *array* of objects to all call `set` on, returning None if any of the objects are duplicates of eachother.

All of these functions take a closure to run on the non-mut or mut reference of the object, and return the result of the closure's invocation - this was just the easiest and most straightforward design that came to mind. If it's burdensome and you'd rather have a RefCell-like Deref wrapper returned, let me know.

Right now you have to manually implement `Trace` for the types you want to make a `Gc<T>` of. This just has to make sure to call `gc.trace(root, c)` for all `Gc<T>` objects reachable from your type. It properly does a recursive visit for the Gc objects with a queue, so don't worry about deep chains of `Gc<T>` objects causing stack overflows. If your type has an interior mutable field containing a `Gc<T>` object, it **must**  have `Trace::MUT = true` set in its impl, for example if your type was `struct Foo { bar: Mutex<Gc<Bar>> }`.

# Example
```rust
struct Node<T: Send + Sync + 'static> {
    pub val: T,
    pub next: Option<Link<T>>,
    pub prev: Option<Link<T>>,
}
impl<T: Send + Sync + 'static> Node<T> {
    fn new(val: T) -> Self {
        Node { val, next: None, prev: None }
    }
}
struct Link<T: Send + Sync + 'static>(Gc<Node<T>>);
// manual impl since derive(Clone) adds an extra T: Clone bound
impl<T: Send + Sync + 'static> Clone for Link<T> {
    fn clone(&self) -> Self {
        Link(self.0.clone())
    }
}


impl<T: Send + Sync + 'static> Link<T> {
    /// Set node to the next link in the list containing self
    fn link_next(&self, token: &mut Owner<Node<T>>, mut node: Node<T>, list: &mut List<T>) {
        let node = if let Some(curr_next) = self.0.get(token, |s| s.next.clone() ) {
            assert!(curr_next.0.as_ptr() != self.0.as_ptr(), "self-reference");
            node.next = Some(curr_next.clone());
            node.prev = Some(self.clone());

            let mut node = Gc::new(node);
            curr_next.0.set(token, |n| n.prev = Some(Link(node.clone())) );
            node
        } else {
            Gc::new(node)
        };
        self.0.set(token, |s| s.next = Some(Link(node.clone())));
        list.len += 1;
    }

    fn unlink(&self, token: &mut Owner<Node<T>>, list: &mut List<T>) {
        let (curr_next, curr_prev) = self.0.get(token, |s| (s.next.clone(), s.prev.clone()) );
        // TODO: do we want a set_two that sorts locks+tries to acquire both, for coherency?
        curr_next.as_ref().map(|c| c.0.set(token, |c| c.prev = curr_prev.clone()));
        curr_prev.as_ref().map(|c| c.0.set(token, |c| c.next = curr_next.clone()));
        if let Some(list_head) = list.head.clone() {
            if list_head.0.as_ptr() == self.0.as_ptr() {
                list.head = curr_next.clone();
            }
        }
        if let Some(list_tail) = list.tail.clone() {
            if list_tail.0.as_ptr() == self.0.as_ptr() {
                list.tail = curr_prev;
            }
        }
        list.len -= 1;
    }
}


struct List<T: Send + Sync + 'static> {
    head: Option<Link<T>>,
    tail: Option<Link<T>>,
    len: usize,
}
impl<T: Send + Sync + 'static> List<T> {
    fn new() -> Self {
        Self { head: None, tail: None, len: 0 }
    }

    fn push_head(&mut self, token: &mut Owner<Node<T>>, val: T) {
        let mut node = Node::new(val);
        node.next = self.head.clone();

        let new_head = Gc::new(node);
        self.head.as_ref().map(|head| head.0.set(token, |i| i.prev = Some(Link(new_head.clone())) ));
        if let None = self.tail {
            self.tail = Some(Link(new_head.clone()));
        }
        self.head = Some(Link(new_head));
        self.len += 1;
    }

    fn push_tail(&mut self, token: &mut Owner<Node<T>>, val: T) {
        let mut node = Node::new(val);
        node.prev = self.tail.clone();
        let new_tail = Gc::new(node);
        self.tail.as_ref().map(|tail| tail.0.set(token, |i| i.next = Some(Link(new_tail.clone())) ));
        if let None = self.head {
            self.head = Some(Link(new_tail.clone()));
        }
        self.tail = Some(Link(new_tail));
        self.len += 1;
    }

    fn get_head(&self) -> Option<Link<T>> {
        self.head.clone()
    }

    fn get(&self, token: &Owner<Node<T>>, i: usize) -> Link<T> {
        let mut head = self.head.clone();
        for _ in 0..i {
            head = head.unwrap().0.get(token, |c| c.next.clone());
        }
        head.unwrap()
    }
}

impl<T: Send + Sync + 'static> Trace for Node<T> {
    fn trace(&self, root: &WeakRoot, c: &mut crate::collector::Collector) {
        self.next.as_ref().map(|n| n.0.trace(root, c));
        self.prev.as_ref().map(|p| p.0.trace(root, c));
    }
}

impl<T: Send + Sync + 'static> Trace for List<T> {
    fn trace(&self, root: &WeakRoot, c: &mut crate::collector::Collector) {
        self.head.as_ref().map(|n| n.0.trace(root, c));
        self.tail.as_ref().map(|p| p.0.trace(root, c));
    }
}

fn main() {
    let mut token = crate::gc::Owner::new();
    // create a double-linked list of numbers...
    let mut list = List::new();
    list.push_tail(&mut token, 1);
    list.push_tail(&mut token, 2);
    list.push_tail(&mut token, 3);
    assert_eq!(crate::gc::number_of_live_objects(), 3);
    drop(list); // drop the list
    // oh no, the refcount cycles are causing memory to leak!
    assert_eq!(crate::gc::number_of_live_objects(), 3);
    Collector::yuga(true); // trigger a collection, blocking on completion
    // yay, all the unreachable objects were freed!
    assert_eq!(crate::gc::number_of_live_objects(), 0);
}
```

[qcell]: https://crates.io/crates/qcell
