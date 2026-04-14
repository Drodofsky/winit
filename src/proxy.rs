use crate::futures::futures::{
    Future, Sink, StreamExt,
    channel::mpsc,
    select,
    task::{Context, Poll},
};
use crate::graphics::shell;
use crate::runtime::Action;
use crate::runtime::window;
use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// An event loop proxy with backpressure that implements `Sink`.
pub struct Proxy<T: 'static> {
    raw: winit::event_loop::EventLoopProxy,
    /// Backpressure slot counter — sends `()` when an action is enqueued.
    sender: mpsc::Sender<()>,
    notifier: mpsc::Sender<usize>,
    /// Shared action buffer, drained by the runner in `proxy_wake_up`.
    pending: Arc<Mutex<VecDeque<Action<T>>>>,
}

impl<T: 'static> fmt::Debug for Proxy<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy").finish_non_exhaustive()
    }
}

impl<T: 'static> Clone for Proxy<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            sender: self.sender.clone(),
            notifier: self.notifier.clone(),
            pending: self.pending.clone(),
        }
    }
}

impl<T: 'static> Proxy<T> {
    const MAX_SIZE: usize = 100;

    /// Creates a new [`Proxy`] from an `EventLoopProxy`.
    ///
    /// Returns the proxy, the shared pending-action buffer (to be stored in
    /// the event-loop runner so it can drain it in `proxy_wake_up`), and the
    /// backpressure worker future that must be spawned.
    pub fn new(
        raw: winit::event_loop::EventLoopProxy,
    ) -> (
        Self,
        Arc<Mutex<VecDeque<Action<T>>>>,
        impl Future<Output = ()>,
    ) {
        let (notifier, mut processed) = mpsc::channel(Self::MAX_SIZE);
        let (sender, mut receiver) = mpsc::channel(Self::MAX_SIZE);
        let proxy = raw.clone();
        let pending = Arc::new(Mutex::new(VecDeque::new()));

        let worker = async move {
            let mut count = 0;

            loop {
                if count < Self::MAX_SIZE {
                    select! {
                        _slot = receiver.select_next_some() => {
                            let _ = proxy.wake_up();
                            count += 1;
                        }
                        amount = processed.select_next_some() => {
                            count = count.saturating_sub(amount);
                        }
                        complete => break,
                    }
                } else {
                    select! {
                        amount = processed.select_next_some() => {
                            count = count.saturating_sub(amount);
                        }
                        complete => break,
                    }
                }
            }
        };

        (
            Self {
                raw,
                sender,
                notifier,
                pending: pending.clone(),
            },
            pending,
            worker,
        )
    }

    /// Sends a value to the event loop.
    ///
    /// Note: This skips the backpressure mechanism with an unbounded
    /// channel. Use sparingly!
    pub fn send(&mut self, value: T) {
        self.send_action(Action::Output(value));
    }

    /// Sends an action to the event loop.
    ///
    /// Note: This skips the backpressure mechanism with an unbounded
    /// channel. Use sparingly!
    pub fn send_action(&mut self, action: Action<T>) {
        self.pending.lock().unwrap().push_back(action);
        let _ = self.sender.try_send(());
        let _ = self.raw.wake_up();
    }

    /// Frees an amount of slots for additional messages to be queued in
    /// this [`Proxy`].
    pub fn free_slots(&mut self, amount: usize) {
        let _ = self.notifier.start_send(amount);
    }
}

impl<T: 'static> Sink<Action<T>> for Proxy<T> {
    type Error = mpsc::SendError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sender.poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, action: Action<T>) -> Result<(), Self::Error> {
        self.pending.lock().unwrap().push_back(action);
        let _ = self.raw.wake_up();
        self.sender.start_send(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.sender.poll_ready(cx) {
            Poll::Ready(Err(ref e)) if e.is_disconnected() => {
                // If the receiver disconnected, we consider the sink to be flushed.
                Poll::Ready(Ok(()))
            }
            x => x,
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.sender.disconnect();
        Poll::Ready(Ok(()))
    }
}

impl<T> shell::Notifier for Proxy<T>
where
    T: Send,
{
    fn request_redraw(&self) {
        self.pending
            .lock()
            .unwrap()
            .push_back(Action::Window(window::Action::RedrawAll));
        let _ = self.raw.wake_up();
    }

    fn invalidate_layout(&self) {
        self.pending
            .lock()
            .unwrap()
            .push_back(Action::Window(window::Action::RelayoutAll));
        let _ = self.raw.wake_up();
    }
}
