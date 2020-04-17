use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "tracing")]
use log::trace;

#[cfg(feature = "tracing")]
static TASK_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct TaskTag {
  #[cfg(feature = "tracing")]
  id: usize,

  schedule_index_hint: AtomicUsize,
}

impl TaskTag {
  pub fn new() -> TaskTag {
    let tag = TaskTag {
      #[cfg(feature = "tracing")]
      id: TASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed),

      schedule_index_hint: AtomicUsize::new(usize::MAX),
    };
    #[cfg(feature = "tracing")]
    trace!("{:?} is created", tag);
    tag
  }

  #[inline]
  pub fn get_schedule_index_hint(&self) -> usize {
    self.schedule_index_hint.load(Ordering::Relaxed)
  }

  #[inline]
  pub fn set_schedule_index_hint(&self, index: usize) {
    // for optimization, load and check first, because atomic store will flush cpu cache
    if self.schedule_index_hint.load(Ordering::Relaxed) != index {
      self.schedule_index_hint.store(index, Ordering::Relaxed);
    }
  }
}

#[cfg(feature = "tracing")]
impl std::fmt::Debug for TaskTag {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&format!("Task({})", self.id))
  }
}

#[cfg(feature = "tracing")]
impl Drop for TaskTag {
  fn drop(&mut self) {
    trace!("{:?} is destroyed", self);
  }
}