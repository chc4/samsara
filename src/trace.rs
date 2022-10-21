use crate::collector::Collector;

pub trait Trace: Send + Sync {
    fn trace(&self, _: &mut Collector) {}
    fn release(&self, _: &mut Collector) {}
}

impl Trace for usize {}
