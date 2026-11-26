use std::{
    collections::VecDeque,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use tokio::sync::oneshot;
use utils::hardware::set_current_thread_below_normal_priority;

type Task = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct Queue {
    visible: VecDeque<Task>,
    background: VecDeque<Task>,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

struct PoolInner {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.closed = true;
        }
        self.shared.ready.notify_all();
        if let Ok(mut threads) = self.threads.lock() {
            for handle in threads.drain(..) {
                if handle.join().is_err() {
                    tracing::error!("render worker thread panicked on shutdown");
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct WorkerPool(Arc<PoolInner>);

impl WorkerPool {
    pub(crate) fn new(worker_threads: usize) -> Self {
        let count = worker_threads.max(1);
        let shared = Arc::new(Shared { queue: Mutex::new(Queue::default()), ready: Condvar::new() });
        let mut threads = Vec::with_capacity(count);
        for index in 0..count {
            let shared = Arc::clone(&shared);
            let builder = thread::Builder::new().name(format!("radia-worker-{index}"));
            let spawn_result = builder.spawn(move || worker_loop(shared));
            match spawn_result {
                Ok(handle) => threads.push(handle),
                Err(error) => tracing::warn!(index, %error, "failed to spawn render worker thread"),
            }
        }
        Self(Arc::new(PoolInner { shared, threads: Mutex::new(threads) }))
    }

    pub(crate) fn spawn<T: Send + 'static>(&self, visible: bool, work: impl FnOnce() -> T + Send + 'static) -> oneshot::Receiver<T> {
        let (tx, rx) = oneshot::channel();
        let task: Task = Box::new(move || {
            drop(tx.send(work()));
        });
        if let Ok(mut queue) = self.0.shared.queue.lock()
            && !queue.closed
        {
            if visible {
                queue.visible.push_back(task);
            } else {
                queue.background.push_back(task);
            }
            self.0.shared.ready.notify_one();
        }
        rx
    }
}

fn worker_loop(shared: Arc<Shared>) {
    set_current_thread_below_normal_priority();
    loop {
        let task = {
            let Ok(mut queue) = shared.queue.lock() else { return };
            loop {
                if let Some(task) = queue.visible.pop_front().or_else(|| queue.background.pop_front()) {
                    break task;
                }
                if queue.closed {
                    return;
                }
                let Ok(guard) = shared.ready.wait(queue) else { return };
                queue = guard;
            }
        };
        if panic::catch_unwind(AssertUnwindSafe(task)).is_err() {
            tracing::error!("render worker task panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};

    use super::*;

    #[tokio::test]
    async fn visible_jobs_run_before_background() {
        let pool = WorkerPool::new(1);
        let gate = Arc::new(Barrier::new(2));
        let blocker_gate = Arc::clone(&gate);
        let blocker = pool.spawn(true, move || {
            blocker_gate.wait();
        });

        let order = Arc::new(Mutex::new(Vec::new()));
        let bg_order = Arc::clone(&order);
        let bg = pool.spawn(false, move || {
            bg_order.lock().expect("order lock").push("background");
        });
        let vis_order = Arc::clone(&order);
        let vis = pool.spawn(true, move || {
            vis_order.lock().expect("order lock").push("visible");
        });

        gate.wait();
        blocker.await.expect("blocker finishes");
        vis.await.expect("visible finishes");
        bg.await.expect("background finishes");
        assert_eq!(*order.lock().expect("order lock"), vec!["visible", "background"]);
    }

    #[tokio::test]
    async fn panicking_task_keeps_worker() {
        let pool = WorkerPool::new(1);
        let panicked = pool.spawn(true, || -> u32 {
            panic!("intentional test panic");
        });
        assert!(panicked.await.is_err(), "panicking task should drop its sender");

        let recovered = pool.spawn(true, || 42_u32);
        assert_eq!(recovered.await.expect("worker should stay alive after panic"), 42);
    }
}
