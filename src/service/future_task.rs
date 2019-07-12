use futures::{
    prelude::*,
    sync::{mpsc, oneshot},
    try_ready,
};
use log::{debug, trace};
use std::collections::HashMap;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::timer::Delay;

use crate::service::SEND_SIZE;

pub(crate) type FutureTaskId = u64;
pub(crate) type BoxedFutureTask = Box<dyn Future<Item = (), Error = ()> + 'static + Send>;

/// A future task manager
pub(crate) struct FutureTaskManager {
    signals: HashMap<FutureTaskId, oneshot::Sender<()>>,
    next_id: FutureTaskId,
    id_sender: mpsc::Sender<FutureTaskId>,
    id_receiver: mpsc::Receiver<FutureTaskId>,
    task_receiver: mpsc::Receiver<BoxedFutureTask>,
    delay: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl FutureTaskManager {
    pub(crate) fn new(
        task_receiver: mpsc::Receiver<BoxedFutureTask>,
        shutdown: Arc<AtomicBool>,
    ) -> FutureTaskManager {
        let (id_sender, id_receiver) = mpsc::channel(SEND_SIZE);
        FutureTaskManager {
            signals: HashMap::default(),
            next_id: 0,
            id_sender,
            id_receiver,
            task_receiver,
            delay: Arc::new(AtomicBool::new(false)),
            shutdown,
        }
    }

    fn add_task(&mut self, task: BoxedFutureTask) {
        let (sender, receiver) = oneshot::channel();
        self.next_id += 1;
        self.signals.insert(self.next_id, sender);

        let task_id = self.next_id;
        let id_sender = self.id_sender.clone();
        let task_wrapper = receiver
            .select2(task)
            .then(move |_| {
                trace!("future task({}) finished", task_id);
                id_sender.send(task_id)
            })
            .map(|_| ())
            .map_err(|_| ());
        trace!("starting future task({})", task_id);
        tokio::spawn(task_wrapper);
    }

    // bounded future task has finished
    fn remove_task(&mut self, id: FutureTaskId) {
        self.signals.remove(&id);
    }

    fn set_delay(&mut self) {
        if !self.delay.load(Ordering::Acquire) {
            self.delay.store(true, Ordering::Release);
            let notify = futures::task::current();
            let delay = self.delay.clone();
            let delay_task =
                Delay::new(Instant::now() + Duration::from_millis(100)).then(move |_| {
                    notify.notify();
                    delay.store(false, Ordering::Release);
                    Ok(())
                });
            tokio::spawn(delay_task);
        }
    }
}

impl Drop for FutureTaskManager {
    fn drop(&mut self) {
        // Because of https://docs.rs/futures/0.1.26/src/futures/sync/oneshot.rs.html#205-209
        // just drop may can't notify the receiver, and receiver will block on runtime, we use send to drop
        // all future task as soon as possible
        self.signals.drain().for_each(|(id, sender)| {
            trace!("future task send stop signal to {}", id);
            let _ = sender.send(());
        })
    }
}

impl Stream for FutureTaskManager {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut task_finished = false;
        let mut id_finished = false;
        for _ in 0..128 {
            if self.shutdown.load(Ordering::SeqCst) {
                debug!("future task finished because service shutdown");
                return Ok(Async::Ready(None));
            }

            match self.task_receiver.poll()? {
                Async::Ready(Some(task)) => self.add_task(task),
                Async::Ready(None) => {
                    debug!("future task receiver finished");
                    return Ok(Async::Ready(None));
                }
                Async::NotReady => {
                    task_finished = true;
                    break;
                }
            }
        }

        for _ in 0..64 {
            if self.shutdown.load(Ordering::SeqCst) {
                debug!("future task finished because service shutdown");
                return Ok(Async::Ready(None));
            }

            match self.id_receiver.poll()? {
                Async::Ready(Some(id)) => self.remove_task(id),
                Async::Ready(None) => {
                    debug!("future task id receiver finished");
                    return Ok(Async::Ready(None));
                }
                Async::NotReady => {
                    id_finished = true;
                    break;
                }
            }
        }

        if !task_finished || !id_finished {
            self.set_delay();
        }

        Ok(Async::NotReady)
    }
}

pub(crate) struct BlockingFutureTask {
    task: BoxedFutureTask,
}

impl BlockingFutureTask {
    pub(crate) fn new(task: BoxedFutureTask) -> BlockingFutureTask {
        BlockingFutureTask { task }
    }
}

impl Future for BlockingFutureTask {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        try_ready!(tokio_threadpool::blocking(|| self.task.poll()).map_err(|_| ()))
    }
}

#[cfg(test)]
mod test {
    use super::{Arc, AtomicBool, BoxedFutureTask, FutureTaskManager};

    use std::{thread, time};

    use futures::{
        future::{empty, lazy},
        prelude::{Future, Stream},
        sink::Sink,
        stream::iter_ok,
        sync::mpsc::channel,
    };

    #[test]
    fn test_manager_drop() {
        let (sender, receiver) = channel(128);
        let shutdown = Arc::new(AtomicBool::new(false));
        let manager = FutureTaskManager::new(receiver, shutdown.clone());
        let tasks = iter_ok(
            (1..100)
                .map(|_| Box::new(empty()) as BoxedFutureTask)
                .collect::<Vec<_>>(),
        );
        let send_task = sender.clone().send_all(tasks);

        let handle = thread::spawn(|| {
            tokio::run(lazy(|| {
                tokio::spawn(manager.for_each(|_| Ok(())).map(|_| ()).map_err(|_| ()));
                tokio::spawn(send_task.map(|_| ()).map_err(|_| ()));
                Ok(())
            }));
        });

        thread::sleep(time::Duration::from_millis(300));
        drop(sender);

        handle.join().unwrap()
    }
}
