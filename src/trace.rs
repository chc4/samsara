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
impl Trace for u16 {}
impl<T: Send + Sync + Trace> Trace for std::sync::Mutex<T> {
    const MUT: bool = true;
    fn trace(&self, root: &WeakRoot, c: &mut Collector) {
        // if we can't acquire a lock, something else is currently using
        // the mutex and thus the object is alive
        drop(self.try_lock().map(|v| (*v).trace(root, c)));
    }
}

//impl<T, C> Trace for HCons<T, C> where T: Trace, C: Trace {
//    const MUT: bool = T::MUT || C::MUT;
//    fn trace(&self, root: &WeakRoot, c: &mut Collector) -> () {
//        let HCons { head, tail } = self;
//        head.trace(root, c);
//        tail.trace(root, c);
//    }
//}
//impl Trace for HNil {
//    fn trace(&self, root: &WeakRoot, c: &mut Collector) -> () {
//        // nothing
//    }
//}
