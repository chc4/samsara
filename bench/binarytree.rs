use samsara::{Gc, WeakRoot, Trace, Collector};

struct TreeNode {
    item: i64,
    left: Option<Gc<Self>>,
    right: Option<Gc<Self>>,
}

impl Trace for TreeNode {
    fn trace(&self, root: &WeakRoot, c: &mut Collector) {
        self.left.as_ref().map(|n| n.trace(root, c));
        self.right.as_ref().map(|n| n.trace(root, c));
    }
}

impl TreeNode {
    fn check_tree(&self) -> usize {
        if self.left.is_none() {
            return 1;
        }

        1 + self.left.as_ref().unwrap().get(|l| l.check_tree()) + self.right.as_ref().unwrap().get(|r| r.check_tree())
    }
}

fn create_tree(depth: i64) -> Gc<TreeNode> {
    let node = if 0 < depth {
        let mut node = TreeNode {
            item: 0,
            left: None,
            right: None,
        };

        node.left = Some(create_tree(depth - 1));
        node.right = Some(create_tree(depth - 1));

        Gc::new(node)
    } else {
        let node = TreeNode {
            item: 0,
            left: None,
            right: None,
        };

        Gc::new(node)
    };
    
    node
}

fn bench_parallel() {

    let mut n = 0;
    if let Some(arg) = std::env::args().skip(1).next() {
        if let Ok(x) = arg.parse::<usize>() {
            n = x;
        }
    }

    let min_depth = 4;
    let max_depth = if n < (min_depth + 2) {
        min_depth + 2
    } else {
        n 
    };

    let start = std::time::Instant::now();
    let stretch_depth = max_depth + 1;

    {
        println!(
            "stretch tree of depth {}\t check: {}",
            stretch_depth,
            create_tree(stretch_depth as _)
                .get(|tree| tree.check_tree())
        );
    }

    let long_lasting_tree = create_tree(max_depth as _);
    use parking_lot::Mutex;
    let results = std::sync::Arc::new(
        (0..(max_depth - min_depth) / 2 + 1)
            .map(|_| Mutex::new(String::new()))
            .collect::<Vec<_>>(),
    );
    std::thread::scope(|scope| {
        let mut d = min_depth;

        while d <= max_depth {
            let depth = d;
            let cloned = results.clone();
            scope.spawn(move || {
                let iterations = 1 << (max_depth - depth + min_depth);
                let mut check = 0;
                for _ in 1..=iterations {
                    let tree_node = create_tree(depth as _);
                    check += tree_node.get(|tree| tree.check_tree());
                }

                *cloned[(depth - min_depth) / 2].lock() = format!(
                    "{}\t trees of depth {}\t check: {}",
                    iterations, depth, check
                );
            });

            d += 2;
        }
    });
    for result in results.iter() {
        println!("{}", *result.lock());
    }
    println!(
        "long lived tree of depth {}\t check: {}",
        max_depth,
        long_lasting_tree.get(|tree| tree.check_tree())
    );

    println!(
        "time: {}ms",
        start.elapsed().as_millis()
    );
}

fn main() {
    bench_parallel();
}
