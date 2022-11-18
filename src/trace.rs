use crate::collector::Collector;
use crate::gc::WeakRoot;

pub trait Trace: Send + Sync {
    fn trace(&self, root: &WeakRoot, c: &mut Collector) {}
}

impl Trace for usize {}
