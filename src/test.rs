use crate::gc::{Gc, WeakRoot};
use crate::trace::Trace;
use crate::gc::number_of_live_objects;
use crate::collector::Collector;
use test_log::test;

//#[cfg(not(all(feature = "shuttle", test)))]
//use {rand, rand::*};

#[cfg(all(feature = "shuttle", test))]
use shuttle::{rand, rand::*};

#[cfg(not(all(feature = "shuttle", test)))]
use {rand, rand::*};

#[derive(Debug)]
struct DirectedGraphNode {
    label: String,
    edges: Vec<Gc<DirectedGraphNode>>,
}

impl Trace for DirectedGraphNode {
    fn trace(&self, root: &WeakRoot, c: &mut crate::collector::Collector) {
        self.edges.iter().map(|p| p.trace(root, c)).for_each(drop);
    }
}

const NODE_COUNT: usize = 1 << 6;
const EDGE_COUNT: usize = 1 << 4;
const SHRINK_DIV: usize = 1 << 3;

struct Node<T: Send + Sync + 'static> {
    pub val: T,
    pub next: Option<Gc<Node<T>>>,
    pub prev: Option<Gc<Node<T>>>,
}
impl<T: Send + Sync + 'static> Node<T> {
    fn new(val: T) -> Self {
        Node { val, next: None, prev: None }
    }

}

impl<T: Send + Sync + 'static> Gc<Node<T>> {
    /// Set node to the next link in the list containing self
    fn link_next(&self, mut node: Node<T>, list: &mut List<T>) {
        let node = if let Some(curr_next) = self.get(|s| s.next.clone() ) {
            assert!(curr_next.as_ptr() != self.as_ptr(), "self-reference");
            node.next = Some(curr_next.clone());
            node.prev = Some(self.clone());

            let mut node = Gc::new(node);
            curr_next.set(|n| n.prev = Some(node.clone()) );
            node
        } else {
            Gc::new(node)
        };
        self.set(|s| s.next = Some(node.clone()));
        list.len += 1;
    }

    fn unlink(&self, list: &mut List<T>) {
        let (curr_next, curr_prev) = self.get(|s| (s.next.clone(), s.prev.clone()) );
        // TODO: do we want a set_two that sorts locks+tries to acquire both, for coherency?
        curr_next.as_ref().map(|c| c.set(|c| c.prev = curr_prev.clone()));
        curr_prev.as_ref().map(|c| c.set(|c| c.next = curr_next.clone()));
        if let Some(list_head) = list.head.clone() {
            if list_head.as_ptr() == self.as_ptr() {
                list.head = curr_next.clone();
            }
        }
        if let Some(list_tail) = list.tail.clone() {
            if list_tail.as_ptr() == self.as_ptr() {
                list.tail = curr_prev;
            }
        }
        list.len -= 1;
    }
}


struct List<T: Send + Sync + 'static> {
    head: Option<Gc<Node<T>>>,
    tail: Option<Gc<Node<T>>>,
    len: usize,
}
impl<T: Send + Sync + 'static> List<T> {
    fn new() -> Self {
        Self { head: None, tail: None, len: 0 }
    }

    fn push_head(&mut self, val: T) {
        let mut node = Node::new(val);
        node.next = self.head.clone();

        let new_head = Gc::new(node);
        self.head.as_ref().map(|head| head.set(|i| i.prev = Some(new_head.clone()) ));
        if let None = self.tail {
            self.tail = Some(new_head.clone());
        }
        self.head = Some(new_head);
        self.len += 1;
    }

    fn push_tail(&mut self, val: T) {
        let mut node = Node::new(val);
        node.prev = self.tail.clone();
        let new_tail = Gc::new(node);
        self.tail.as_ref().map(|tail| tail.set(|i| i.next = Some(new_tail.clone()) ));
        if let None = self.head {
            self.head = Some(new_tail.clone());
        }
        self.tail = Some(new_tail);
        self.len += 1;
    }

    fn get_head(&self) -> Option<Gc<Node<T>>> {
        self.head.clone()
    }

    fn get(&self, i: usize) -> Gc<Node<T>> {
        let mut head = self.head.clone();
        for _ in 0..i {
            head = head.unwrap().get(|c| c.next.clone());
        }
        head.unwrap()
    }
}

impl<T: Send + Sync + 'static> Trace for Node<T> {
    fn trace(&self, root: &WeakRoot, c: &mut crate::collector::Collector) {
        self.next.as_ref().map(|n| n.trace(root, c));
        self.prev.as_ref().map(|p| p.trace(root, c));
    }
}

impl<T: Send + Sync + 'static> Trace for List<T> {
    fn trace(&self, root: &WeakRoot, c: &mut crate::collector::Collector) {
        self.head.as_ref().map(|n| n.trace(root, c));
        self.tail.as_ref().map(|p| p.trace(root, c));
    }
}

fn choose<T>(vec: &Vec<T>) -> &T {
    &vec[self::rand::thread_rng().gen_range(0..vec.len())]
}

fn test_graph() {
    println!("Creating nodes...");
    let mut nodes = Vec::new();

    for i in 0..=NODE_COUNT {
        nodes.push(Gc::new(DirectedGraphNode {
            label: format!("Node {}", i),
            edges: Vec::new(),
        }));
    }

    println!("Adding edges...");
    for i in 0..=EDGE_COUNT {
        println!("{}", i);
        Collector::maybe_yuga();
        let a = choose(&nodes);
        let b = choose(&nodes);
        if a.as_ptr() == b.as_ptr() { continue; }

        println!("add edge {:x} -> {:x}", a as *const _ as usize, b as *const _ as usize);
        a.set(|a|{ Collector::maybe_yuga(); a.edges.push(Gc::clone(&b)) });
    }

    println!("Doing the shrink...");
    for i in 0..NODE_COUNT {
        println!("shrink {}", i);
        if i % SHRINK_DIV == 0 {
            //nodes.truncate(NODE_COUNT - i);
            nodes.remove(self::rand::thread_rng().gen_range(0..nodes.len()));
            Collector::maybe_yuga();
            let live = number_of_live_objects();
            println!("Now have {} datas and {} nodes", live, nodes.len());
            for node in &nodes {
                let label = node.get(|n| n.label.len() );
                assert_ne!(label, 0);
            }
            // TODO: Add an assert here. this isn't correct: objects can
            // still be alive due to live Weak<T> queued on the channel.
            //assert_eq!(nodes.len(), live);
        }
    }
    println!("done");
}

const LIST_COUNT: usize = 1 << 4;
const ACTION_COUNT: usize = 1 << 1;
fn test_list() {
    println!("Creating list...");
    let mut list = List::new();
    for i in 0..=LIST_COUNT {
        list.push_tail(i);
    }

    println!("Inserting randomly");
    for i in 0..ACTION_COUNT {
        match self::rand::thread_rng().gen_range(0..=2) {
            //0 => list.get(thread_rng().gen_range(0, list.len)).unlink(&mut list),
            1 => list.get(self::rand::thread_rng().gen_range(0..=list.len-1)).link_next(Node::new(i), &mut list),
            _ => (),
        }
        Collector::maybe_yuga();
    }

    if let Some(head) = list.get_head() {
        let info = head.get(|n| (n.val, n.next.is_some(), n.prev.is_some()) );
        println!("{}, next {} prev {}", info.0, info.1, info.2);
    }
}

fn mini_list() {
    println!("creating list");
    let mut list = List::new();
    for i in 0..3 {
        list.push_tail(i);
    }
    Collector::yuga();
    println!("------------ BUG HERE");
    let n = list.get(1);
    n.link_next(Node::new(4), &mut list);
    drop(n);
    println!("------------ BUG END");
    /*let mut curr = list.head;
    loop {
        if let Some(ref link) = curr {
            println!("link {}", link.get(|l| l.val));
            curr = link.get(|l| l.next.clone());
        } else {
            break;
        }
    }*/
}


#[cfg(not(all(feature = "shuttle", test)))]
mod test {
    use super::*;

    #[test]
    fn normal_test_list() {
        test_list();
        Collector::yuga();
        assert_eq!(crate::gc::number_of_live_objects(), 0);
    }
}

#[cfg(all(feature = "shuttle", test))]
mod test {
    use super::*;
    fn exhaustive<F: Fn() + Send + Sync + 'static>(f: F) {
        use shuttle::scheduler::DfsScheduler;
        let dfs = DfsScheduler::new(None, true);
        let mut runner = shuttle::PortfolioRunner::new(true, shuttle::Config::new());
        runner.add(dfs);
        runner.run(f);
    }

    fn random<F: Fn() + Send + Sync + 'static>(f: F, iters: usize) {
        use shuttle::scheduler::RandomScheduler;
        let dfs = RandomScheduler::new(iters);
        let mut config = shuttle::Config::new();
        config.max_steps = shuttle::MaxSteps::FailAfter(10_000);
        let mut runner = shuttle::PortfolioRunner::new(true, config);
        runner.add(dfs);
        runner.run(f);
    }

    #[self::test]
    fn shuttle_test_mini() {
        random(|| {
            mini_list();
            Collector::yuga();
            assert_eq!(crate::gc::number_of_live_objects(), 0);
        }, 100);
    }

    #[self::test]
    fn shuttle_test_list() {
        random(|| {
            test_list();
            Collector::yuga();
            assert_eq!(crate::gc::number_of_live_objects(), 0);
        }, 100);
    }

    #[self::test]
    fn shuttle_fail_list() {
        shuttle::replay(|| {
            test_list();
            Collector::yuga();
            assert_eq!(crate::gc::number_of_live_objects(), 0);
        }, "91019004a4b98099e9c7b29a59008a00000080000000020000200000200000000800000200000002008000000008000008000008000020000000020000020000080000f0170150401544510411551410040000155054044114010005554414111445220008aa08a228a28aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa00");
    }

    #[self::test]
    fn shuttle_test_graph() {
        shuttle::check_random(|| {
            test_graph();
            Collector::yuga();
            assert_eq!(crate::gc::number_of_live_objects(), 0);
        }, 100);
    }

    #[self::test]
    fn shuttle_fail_graph() {
        shuttle::replay(|| {
            test_graph();
        }, "");
    }
}
