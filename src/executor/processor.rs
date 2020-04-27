use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_channel::{bounded, Receiver, Sender};
use crossbeam_deque::{Injector, Steal, Worker};
use crossbeam_utils::Backoff;

#[cfg(feature = "tracing")]
use log::trace;

use super::machine::Machine;
use super::system::System;
use super::Task;

/// Processor is the one who run the task
pub struct Processor {
    pub index: usize,

    /// current machine that hold the processor
    machine_id: AtomicUsize,

    /// for blocking detection
    /// usize::MAX mean the processor is sleeping
    last_seen: AtomicUsize,

    /// global queue dedicated to this processor
    injector: Injector<Task>,
    injector_notif: Sender<()>,
    injector_notif_recv: Receiver<()>,
}

pub struct RunContext<'a> {
    pub system: &'a System,
    pub machine: &'a Machine,
    pub worker: &'a Worker<Task>,
}

impl Processor {
    pub fn new(index: usize) -> Processor {
        // channel with buffer size 1 to not miss a notification
        let (injector_notif, injector_notif_recv) = bounded(1);

        #[allow(clippy::let_and_return)]
        let processor = Processor {
            index,

            last_seen: AtomicUsize::new(usize::MAX),

            injector: Injector::new(),
            injector_notif,
            injector_notif_recv,

            machine_id: AtomicUsize::new(usize::MAX),
        };

        #[cfg(feature = "tracing")]
        trace!("{:?} is created", processor);

        processor
    }

    pub fn run(&self, ctx: &RunContext) {
        let RunContext {
            system,
            machine,
            worker,
            ..
        } = ctx;

        self.machine_id.store(machine.id, Ordering::Relaxed);
        self.last_seen.store(system.now(), Ordering::Relaxed);

        #[cfg(feature = "tracing")]
        crate::thread_pool::THREAD_ID.with(|tid| {
            trace!("{:?} is now running on {:?} on {:?}", self, machine, tid);
        });

        // Number of runs in a row before the global queue is inspected.
        const MAX_RUNS: usize = 64;
        let mut run_counter = 0;

        let sleep_backoff = Backoff::new();

        #[cfg(feature = "tracing")]
        let mut last_task_info = String::new();

        'main: loop {
            // mark this processor still healthy
            self.last_seen.store(system.now(), Ordering::Relaxed);

            macro_rules! run_task {
                ($task:ident) => {{
                    if self.still_on_machine(machine) {
                        #[cfg(feature = "tracing")]
                        {
                            last_task_info = format!("{:?}", $task.tag());
                            trace!("{} is running on {:?}", last_task_info, self);
                        }

                        // update the tag, so this task will be push to this processor again
                        $task.tag().set_schedule_index_hint(self.index);
                        $task.run();

                        #[cfg(feature = "tracing")]
                        {
                            trace!("{} is done running on {:?}", last_task_info, self);
                        }
                    } else {
                        // there is possibility that (*) is skipped because of race condition,
                        // put it back in global queue
                        system.push($task);
                    }

                    // (*) if the processor is running in another machine after we run the task,
                    // that mean the task is blocking, just exit
                    if !self.still_on_machine(machine) {
                        #[cfg(feature = "tracing")]
                        trace!(
                            "{} was blocking {:?} when on {:?}",
                            last_task_info,
                            self,
                            machine,
                        );
                        return;
                    }

                    run_counter += 1;
                    continue 'main;
                }};
            }

            macro_rules! get_tasks {
                () => {{
                    run_counter = 0;
                    let _ = self.injector_notif_recv.try_recv(); // flush the notification channel
                    if let Some(task) = system.pop(self.index, worker) {
                        run_task!(task);
                    }
                }};
            }

            if run_counter >= MAX_RUNS {
                get_tasks!();
            }

            // run all task in the worker
            if let Some(task) = worker.pop() {
                run_task!(task);
            }

            // at this point, the worker is empty

            // 1. pop from global queue
            get_tasks!();

            // 2. steal from others
            if let Some(task) = system.steal(&worker) {
                run_task!(task);
            }

            // 3.a. no more task for now, just sleep
            self.sleep(ctx, &sleep_backoff);

            // 3.b. after sleep, pop from global queue
            get_tasks!();
        }
    }

    fn sleep(&self, ctx: &RunContext, backoff: &Backoff) {
        let RunContext { system, .. } = ctx;
        if backoff.is_completed() {
            #[cfg(feature = "tracing")]
            trace!("{:?} entering sleep", self);

            #[cfg(feature = "tracing")]
            defer! {
              trace!("{:?} leaving sleep", self);
            }

            self.last_seen.store(usize::MAX, Ordering::Relaxed);
            self.injector_notif_recv.recv().unwrap();
            self.last_seen.store(system.now(), Ordering::Relaxed);
            system.sysmon_wake_up();

            backoff.reset();
        } else {
            backoff.snooze();
        }
    }

    #[inline(always)]
    pub fn still_on_machine(&self, machine: &Machine) -> bool {
        self.machine_id.load(Ordering::Relaxed) == machine.id
    }

    /// will return usize::MAX when processor is idle (always seen in the future)
    #[inline(always)]
    pub fn get_last_seen(&self) -> usize {
        self.last_seen.load(Ordering::Relaxed)
    }

    /// return true if wake up signal is delivered
    pub fn wake_up(&self) -> bool {
        self.injector_notif.try_send(()).is_ok()
    }

    /// return true if wake up signal is delivered
    pub fn push_then_wake_up(&self, t: Task) -> bool {
        self.injector.push(t);
        self.wake_up()
    }

    pub fn pop(&self, dest: &Worker<Task>) -> Option<Task> {
        // retry until success or empty
        std::iter::repeat_with(|| self.injector.steal_batch_and_pop(dest))
            .filter(|s| !matches!(s, Steal::Retry))
            .map(|s| match s {
                Steal::Success(task) => Some(task),
                Steal::Empty => None,
                Steal::Retry => unreachable!(), // already filtered
            })
            .next()
            .unwrap()
    }
}

impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("Processor({})", self.index))
    }
}
