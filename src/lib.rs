mod trace;
mod gc;
mod collector;
mod tracker;
mod test;

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(not(all(feature = "shuttle", test)))]
mod tests {
    use super::*;
    use gc::{Gc, WeakGc};
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
        fn trace(&self, root: &dyn WeakGc, c: &mut Collector) {
            self.bar.trace(root, c);
            self.foo1.trace(root, c);
            self.foo2.trace(root, c);
            self.foo3.trace(root, c);
        }
    }

    pub struct Bar {
        b: usize,
        bar: Gc<Bar>,
    }

    impl Trace for Bar {
        fn trace(&self, root: &dyn WeakGc, c: &mut Collector) {
            println!("trace {}", self.b);
            self.bar.trace(root, c);
        }
    }

    impl Drop for Bar {
        fn drop(&mut self) {
            println!("drop {}", self.b);
        }
    }

    #[test]
    fn simple_bar() {
        let item = Gc::new(Bar { b: 0, bar: Gc::empty() });
        assert_eq!(item.get(|i| i.b), 0);
        item.set(|i| i.b = 1);
        assert_eq!(item.get(|i| i.b), 1);
    }

    #[test]
    fn simple_foo() {
        let bar = Gc::new(Bar { b: 0, bar: Gc::empty() });
        let foo = Gc::new(Foo { a: 0,
            bar: bar.clone(),
            foo1: Gc::empty(),
            foo2: Gc::empty(),
            foo3: Gc::empty(),
        });
        println!("1");
        assert_eq!(bar.get(|i| i.b), 0);
        bar.set(|i| i.b = 1);
        println!("2");
        assert_eq!(bar.get(|i| i.b), 1);
        assert_eq!(foo.get(|i| i.bar.get(|i2| i2.b)), 1);
        println!("3");
        drop(foo);
        println!("4");
        Collector::yuga();
        println!("5");
        assert_eq!(bar.get(|i| i.b), 1);
        drop(bar);
        println!("6");
        Collector::yuga();
        println!("7");
    }

    #[test]
    fn simple_cyclic() {
        let a1 = Gc::new(Bar { b: 0, bar: Gc::empty() });
        let a2 = Gc::new(Bar { b: 1, bar: Gc::empty() });
        let a3 = Gc::new(Bar { b: 2, bar: Gc::empty() });
        a1.set(|a| a.bar = a2.clone());
        a2.set(|a| a.bar = a3.clone());
        a3.set(|a| a.bar = a1.clone());
        println!("1");
        drop(a1);
        Collector::yuga();
        println!("2");
        drop(a2);
        Collector::yuga();
        println!("3");
        drop(a3);
        Collector::yuga();
    }

    #[test]
    fn held_cyclic() {
        let a1 = Gc::new(Bar { b: 0, bar: Gc::empty() });
        let a2 = Gc::new(Bar { b: 1, bar: Gc::empty() });
        let a3 = Gc::new(Bar { b: 2, bar: Gc::empty() });
        a1.set(|a| a.bar = a2.clone());
        a2.set(|a| a.bar = a3.clone());
        a3.set(|a| a.bar = a1.clone());
        println!("1");
        drop(a1);
        drop(a2);
        println!("2");
        a3.set(|a| {
            Collector::yuga();
            drop(a);
        });
    }

    #[test]
    fn two_cycles() {
        struct Join { a: Gc<Bar>, b: Gc<Bar> };
        let join = {
            let a1 = Gc::new(Bar { b: 0, bar: Gc::empty() });
            let a2 = Gc::new(Bar { b: 1, bar: Gc::empty() });
            let a3 = Gc::new(Bar { b: 2, bar: Gc::empty() });
            a1.set(|a| a.bar = a2.clone());
            a2.set(|a| a.bar = a3.clone());
            a3.set(|a| a.bar = a1.clone());

            let b1 = Gc::new(Bar { b: 3, bar: Gc::empty() });
            let b2 = Gc::new(Bar { b: 4, bar: Gc::empty() });
            let b3 = Gc::new(Bar { b: 5, bar: Gc::empty() });
            b1.set(|b| b.bar = b2.clone());
            b2.set(|b| b.bar = b3.clone());
            b3.set(|b| b.bar = b1.clone());

            Join { a: a1.clone(), b: b1.clone() }
        };

        println!("1");
        Collector::yuga();
        println!("2");
        drop(join);
        Collector::yuga();
    }
}
