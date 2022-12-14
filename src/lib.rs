#![feature(array_methods)]
mod trace;
mod gc;
mod collector;
mod tracker;
#[cfg(test)]
mod test;

pub use gc::{Gc, WeakRoot, Owner};
pub use trace::Trace;
pub use collector::Collector;

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(not(all(feature = "shuttle", test)))]
mod tests {
    use super::*;
    use gc::{Gc, WeakRoot};
    use collector::Collector;
    use trace::Trace;

    pub struct Foo {
        a: usize,
        bar: Gc<Bar>,
        foo1: Gc<Foo>,
        foo2: Gc<Foo>,
        foo3: Gc<Foo>,
    }

    impl Trace for Foo {
        fn trace(&self, root: &WeakRoot, c: &mut Collector) {
            self.bar.trace(root, c);
            self.foo1.trace(root, c);
            self.foo2.trace(root, c);
            self.foo3.trace(root, c);
        }
    }

    pub struct Bar {
        b: usize,
        bar: Vec<Gc<Bar>>,
    }

    impl Trace for Bar {
        fn trace(&self, root: &WeakRoot, c: &mut Collector) {
            println!("trace {}", self.b);
            self.bar.iter().map(|e| e.trace(root, c)).for_each(drop);
        }
    }

    impl Drop for Bar {
        fn drop(&mut self) {
            println!("drop {}", self.b);
        }
    }

    #[test]
    fn simple_bar() {
        let mut token = Owner::wait_for_new();
        let item = Gc::new(Bar { b: 0, bar: vec![] });
        assert_eq!(item.get(&token, |i| i.b), 0);
        item.set(&mut token, |i| i.b = 1);
        assert_eq!(item.get(&token, |i| i.b), 1);
    }

    #[test]
    fn simple_foo() {
        let mut token = Owner::wait_for_new();
        let mut foo_token = Owner::wait_for_new();
        let bar = Gc::new(Bar { b: 0, bar: vec![] });
        let foo = Gc::new(Foo { a: 0,
            bar: bar.clone(),
            foo1: Gc::empty(),
            foo2: Gc::empty(),
            foo3: Gc::empty(),
        });
        println!("1");
        assert_eq!(bar.get(&token, |i| i.b), 0);
        bar.set(&mut token, |i| i.b = 1);
        println!("2");
        assert_eq!(bar.get(&token, |i| i.b), 1);
        assert_eq!(foo.get(&foo_token, |i| i.bar.get(&token, |i2| i2.b)), 1);
        println!("3");
        drop(foo);
        println!("4");
        Collector::maybe_yuga();
        println!("5");
        assert_eq!(bar.get(&token, |i| i.b), 1);
        drop(bar);
        println!("6");
        Collector::yuga(true);
        println!("7");
        assert_eq!(gc::number_of_live_objects(), 0);
    }

    #[test]
    fn simple_cyclic() {
        let mut token = Owner::wait_for_new();
        let a1 = Gc::new(Bar { b: 0, bar: vec![] });
        let a2 = Gc::new(Bar { b: 1, bar: vec![] });
        let a3 = Gc::new(Bar { b: 2, bar: vec![] });
        a1.set(&mut token, |a| a.bar = vec![a2.clone()]);
        a2.set(&mut token, |a| a.bar = vec![a3.clone()]);
        a3.set(&mut token, |a| a.bar = vec![a1.clone()]);
        println!("1");
        drop(a1);
        Collector::maybe_yuga();
        println!("2");
        drop(a2);
        Collector::maybe_yuga();
        println!("3");
        drop(a3);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 0);
    }

    #[test]
    fn held_cyclic() {
        let mut token = Owner::wait_for_new();
        let a1 = Gc::new(Bar { b: 0, bar: vec![] });
        let a2 = Gc::new(Bar { b: 1, bar: vec![] });
        let a3 = Gc::new(Bar { b: 2, bar: vec![] });
        a1.set(&mut token, |a| a.bar = vec![a2.clone()]);
        a2.set(&mut token, |a| a.bar = vec![a3.clone()]);
        a3.set(&mut token, |a| a.bar = vec![a1.clone()]);
        println!("1");
        drop(a1);
        drop(a2);
        println!("2");
        a3.set(&mut token, |a| {
            Collector::yuga(true);
            drop(a);
        });
    }

    #[test]
    fn downstream_of_cyclic_1() {
        let mut token = Owner::wait_for_new();
        let a1 = Gc::new(Bar { b: 0, bar: vec![] });
        let a2 = Gc::new(Bar { b: 1, bar: vec![] });
        let a3 = Gc::new(Bar { b: 2, bar: vec![] });
        let live = Gc::new(Bar { b: 3, bar: vec![] });
        a1.set(&mut token, |a| a.bar = vec![a2.clone()]);
        a2.set(&mut token, |a| a.bar = vec![a3.clone()]);
        a3.set(&mut token, |a| a.bar = vec![a1.clone()]);
        println!("1");
        drop(a1);
        drop(a2);
        println!("2");
        drop(a3);
        drop(live.clone());
        println!("3");
        Collector::yuga(true);
        println!("4");
        assert_eq!(live.get(&token, |l| l.b), 3);
        assert_eq!(gc::number_of_live_objects(), 1);
    }

    #[test]
    fn downstream_of_cyclic_2() {
        let mut token = Owner::wait_for_new();
        let a1 = Gc::new(Bar { b: 0, bar: vec![] });
        let a2 = Gc::new(Bar { b: 1, bar: vec![] });
        let a3 = Gc::new(Bar { b: 2, bar: vec![] });
        let live = Gc::new(Bar { b: 3, bar: vec![] });
        a1.set(&mut token, |a| a.bar = vec![a3.clone()]);
        a2.set(&mut token, |a| a.bar = vec![a1.clone()]);
        a3.set(&mut token, |a| a.bar = vec![a2.clone()]);
        println!("1");
        drop(a1);
        drop(a2);
        println!("2");
        drop(a3);
        drop(live.clone());
        println!("3");
        Collector::yuga(true);
        println!("4");
        assert_eq!(live.get(&token, |l| l.b), 3);
        assert_eq!(gc::number_of_live_objects(), 1);
    }

    #[test]
    fn two_cycles_join() {
        let mut token = Owner::wait_for_new();
        struct Join { a: Gc<Bar>, b: Gc<Bar> }
        impl Trace for Join {
            fn trace(&self, root: &WeakRoot, c: &mut Collector) {
                self.a.trace(root, c);
                self.b.trace(root, c);
            }
        }
        let join = {
            let a1 = Gc::new(Bar { b: 0, bar: vec![] });
            let a2 = Gc::new(Bar { b: 1, bar: vec![] });
            let a3 = Gc::new(Bar { b: 2, bar: vec![] });
            a1.set(&mut token, |a| a.bar = vec![a2.clone()]);
            a2.set(&mut token, |a| a.bar = vec![a3.clone()]);
            a3.set(&mut token, |a| a.bar = vec![a1.clone()]);

            let b1 = Gc::new(Bar { b: 3, bar: vec![] });
            let b2 = Gc::new(Bar { b: 4, bar: vec![] });
            let b3 = Gc::new(Bar { b: 5, bar: vec![] });
            b1.set(&mut token, |b| b.bar = vec![b2.clone()]);
            b2.set(&mut token, |b| b.bar = vec![b3.clone()]);
            b3.set(&mut token, |b| b.bar = vec![b1.clone()]);

            Gc::new(Join { a: a1.clone(), b: b1.clone() })
        };

        println!("1");
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 7);
        println!("2");
        drop(join);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 0);
    }

    #[test]
    fn two_cycles_meet() {
        let mut token = Owner::wait_for_new();
        let mut root_token = Owner::wait_for_new();
        let root = {
            let meet = Gc::new(Bar { b: 123, bar: vec![] });
            let a1 = Gc::new(Bar { b: 0, bar: vec![] });
            let a2 = Gc::new(Bar { b: 1, bar: vec![] });
            let a3 = Gc::new(Bar { b: 2, bar: vec![] });
            a1.set(&mut token, |a| a.bar = vec![a2.clone()]);
            a2.set(&mut token, |a| a.bar = vec![a3.clone()]);
            a3.set(&mut token, |a| a.bar = vec![a1.clone(), meet.clone()]);

            let b1 = Gc::new(Bar { b: 3, bar: vec![] });
            let b2 = Gc::new(Bar { b: 4, bar: vec![] });
            let b3 = Gc::new(Bar { b: 5, bar: vec![] });
            b1.set(&mut token, |b| b.bar = vec![b2.clone()]);
            b2.set(&mut token, |b| b.bar = vec![b3.clone()]);
            b3.set(&mut token, |b| b.bar = vec![b1.clone(), meet.clone()]);
            b2
        };

        println!("1");
        root.get(&root_token, move |r| r.bar[0].set(&mut token, |b| Collector::yuga(true)));
        assert_eq!(gc::number_of_live_objects(), 4);
        println!("2");
        drop(root);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 0);
    }

    fn new(n: usize) -> Gc<Bar> { Gc::new(Bar { b : n, bar: vec![] }) }

    #[test]
    fn weird() {
        let mut token = Owner::wait_for_new();
        // a0 -> a1 -> a2 -> a4 -> a5
        //       a3 -^
        let a0 = new(0);
        let a1 = new(1);
        a0.set(&mut token, |a| a.bar.push(Gc::clone(&a1)));
        let a2 = new(2);
        a1.set(&mut token, |a| a.bar.push(Gc::clone(&a2)));
        let a3 = new(3);
        let a4 = new(4);
        a3.set(&mut token, |a| a.bar.push(Gc::clone(&a4)));
        a1.set(&mut token, |a| a.bar.push(Gc::clone(&a4)));
        let a5 = new(5);
        a4.set(&mut token, |a| a.bar.push(Gc::clone(&a5)));
        drop(a0.clone());
        drop(a1.clone());
        drop(a2.clone());
        drop(a3.clone());
        drop(a4);
        drop(a5);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 6);
    }

    #[test]
    fn two_edges() {
        let mut token = Owner::wait_for_new();
        let a0 = new(0);
        let a1 = new(1);
        a0.set(&mut token, |a|{ a.bar.push(Gc::clone(&a1)); a.bar.push(Gc::clone(&a1)); });
        drop(a1);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 1);
    }

    #[test]
    fn self_reference() {
        let mut token = Owner::wait_for_new();
        let a0 = new(0);
        a0.set(&mut token, |a| a.bar.push(Gc::clone(&a0)));
        drop(a0);
        Collector::yuga(true);
        assert_eq!(gc::number_of_live_objects(), 0);
    }
}
