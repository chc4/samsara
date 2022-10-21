mod trace;
mod gc;
mod collector;

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;
    use gc::Gc;
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
        fn trace(&self, c: &mut Collector) {
            c.visit(&self.bar);
            c.visit(&self.foo1);
            c.visit(&self.foo2);
            c.visit(&self.foo3);
        }
    }

    pub struct Bar {
        b: usize,
        bar: Gc<Bar>,
    }

    impl Trace for Bar {
        fn trace(&self, c: &mut Collector) {
            c.visit(&self.bar);
        }
    }

    #[test]
    fn simple_bar() {
        let item = Gc::new(Bar { b: 0, bar: Gc::empty() });
        assert_eq!(item.get().b, 0);
        item.set(|mut i| i.b = 1);
        assert_eq!(item.get().b, 1);
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
        assert_eq!(bar.get().b, 0);
        bar.set(|mut i| i.b = 1);
        assert_eq!(bar.get().b, 1);
        assert_eq!(foo.get().bar.get().b, 1);
        drop(foo);
        Collector::yuga();
        assert_eq!(bar.get().b, 1);
        drop(bar);
        Collector::yuga();
    }
}
