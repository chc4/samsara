
pub struct Foo {
    a: usize,
    bar: Gc<Bar>,
    foo1: Gc<Foo>,
    foo2: Gc<Foo>,
    foo3: Gc<Foo>,
}

impl Trace for Foo {
    fn trace(&self, c: &mut Collector) {
        c.visit(self.bar);
        c.visit(self.foo1);
        c.visit(self.foo2);
        c.visit(self.foo3);
    }
}

pub struct Bar {
    b: usize,
    bar: Gc<Bar>,
}

impl Trace for Bar {
    fn trace(&self, c: &mut Collector) {
        c.visit(self.bar);
    }
}
