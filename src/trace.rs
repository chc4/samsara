use crate::collector::Collector;
use crate::gc::WeakRoot;

pub trait Trace: Send + Sync {
    /// Associated flag for if this tracable object is interior mutable in a way
    /// that would allow for moving a Gc<T> reference from a &-reference.
    /// This should be true if, for example, the object transitively contains an
    /// UnsafeCell<T1> where T1 transitively contains a Gc<T2>.
    const MUT: bool = false;
    fn trace(&self, root: &WeakRoot, c: &mut Collector) {}
}

impl Trace for usize {}
