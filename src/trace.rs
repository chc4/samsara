use crate::collector::Collector;
use crate::gc::WeakGc;

pub trait Trace: Send + Sync {
    fn trace(&self, root: &dyn WeakGc, c: &mut Collector) {}
    fn release(&self, _: &mut Collector) {}
}

impl Trace for usize {}
