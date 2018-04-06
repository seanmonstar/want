#![doc(html_root_url = "https://docs.rs/want/0.0.2")]
#![deny(warnings)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

//! A Futures channel-like utility to signal when a value is wanted.
//!
//! Futures are supposed to be lazy, and only starting work if `Future::poll`
//! is called. The same is true of `Stream`s, but when using a channel as
//! a `Stream`, it can be hard to know if the receiver is ready for the next
//! value.
//!
//! Put another way, given a `(tx, rx)` from `futures::sync::mpsc::channel()`,
//! how can the sender (`tx`) know when the receiver (`rx`) actually wants more
//! work to be produced? Just because there is room in the channel buffer
//! doesn't mean the work would be used by the receiver.
//!
//! This is where something like `want` comes in. Added to a channel, you can
//! make sure that the `tx` only creates the message and sends it when the `rx`
//! has `poll()` for it, and the buffer was empty.

extern crate futures;
#[macro_use]
extern crate log;
extern crate try_lock;

use std::fmt;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::{Async, Poll};
use futures::task::{self, Waker};

use try_lock::TryLock;

/// Create a new `want` channel.
pub fn new() -> (Giver, Taker) {
    let inner = Arc::new(Inner {
        state: AtomicUsize::new(State::Idle.into()),
        waker: TryLock::new(None),
    });
    let inner2 = inner.clone();
    (
        Giver {
            inner: inner,
        },
        Taker {
            inner: inner2,
        },
    )
}

/// An entity that gives a value when wanted.
pub struct Giver {
    inner: Arc<Inner>,
}

/// An entity that wants a value.
pub struct Taker {
    inner: Arc<Inner>,
}

/// The `Taker` has canceled its interest in a value.
pub struct Closed {
    _inner: (),
}

#[derive(Clone, Copy, Debug)]
enum State {
    Idle,
    Want,
    Give,
    Closed,
}

impl From<State> for usize {
    fn from(s: State) -> usize {
        match s {
            State::Idle => 0,
            State::Want => 1,
            State::Give => 2,
            State::Closed => 3,
        }
    }
}

impl From<usize> for State {
    fn from(num: usize) -> State {
        match num {
            0 => State::Idle,
            1 => State::Want,
            2 => State::Give,
            3 => State::Closed,
            _ => unreachable!("unknown state: {}", num),
        }
    }
}

struct Inner {
    state: AtomicUsize,
    waker: TryLock<Option<Waker>>,
}

// ===== impl Giver ======

impl Giver {
    /// Poll whether the `Taker` has registered interest in another value.
    ///
    /// - If the `Taker` has called `want()`, this returns `Async::Ready(())`.
    /// - If the `Taker` has not called `want()` since last poll, this
    ///   returns `Async::NotReady`, and parks the current task to be notified
    ///   when the `Taker` does call `want()`.
    /// - If the `Taker` has canceled (or dropped), this returns `Closed`.
    pub fn poll_want(&mut self, cx: &mut task::Context) -> Poll<(), Closed> {
        loop {
            let state = self.inner.state.load(Ordering::SeqCst).into();
            match state {
                State::Want => {
                    trace!("poll_want: taker wants!");
                    // only set to IDLE if it is still Want
                    self.inner.state.compare_and_swap(
                        State::Want.into(),
                        State::Idle.into(),
                        Ordering::SeqCst,
                    );
                    return Ok(Async::Ready(()));
                },
                State::Closed => {
                    trace!("poll_want: closed");
                    return Err(Closed { _inner: () });
                },
                State::Idle | State::Give => {
                    // Taker doesn't want anything yet, so park.
                    if let Some(mut locked) = self.inner.waker.try_lock() {

                        // While we have the lock, try to set to GIVE.
                        let old = self.inner.state.compare_and_swap(
                            state.into(),
                            State::Give.into(),
                            Ordering::SeqCst,
                        );
                        // If it's still the first state (Idle or Give), park current task.
                        if old == state.into() {
                            trace!("poll_want: taker doesn't want, parking task");
                            let current_waker = cx.waker();
                            let park = locked.as_ref()
                                .map(|t| t.will_wake(current_waker))
                                .unwrap_or(true);
                            if park {
                                mem::replace(&mut *locked, Some(current_waker.clone()))
                                    .map(|prev_task| {
                                        // there was an old task parked here.
                                        // it might be waiting to be notified,
                                        // so poke it before dropping.
                                        prev_task.wake();
                                    });
                            }
                            return Ok(Async::Pending)
                        }
                        // Otherwise, something happened! Go around the loop again.
                    } else {
                        // if we couldn't take the lock, then a Taker has it.
                        // The *ONLY* reason is because it is in the process of notifying us
                        // of its want.
                        //
                        // We need to loop again to see what state it was changed to.
                    }
                },
            }
        }
    }

    /// Check if the `Taker` has called `want()` without parking a task.
    ///
    /// This is safe to call outside of a futures task context, but other
    /// means of being notified is left to the user.
    #[inline]
    pub fn is_wanting(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) == State::Want.into()
    }

    /// Check if the `Taker` has canceled interest without parking a task.
    #[inline]
    pub fn is_canceled(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) == State::Closed.into()
    }
}

impl fmt::Debug for Giver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Giver")
            .field("state", &self.inner.state())
            .finish()
    }
}

// ===== impl Taker ======

impl Taker {
    /// Signal to the `Giver` that the want is canceled.
    ///
    /// This is useful to tell that the channel is closed if you cannot
    /// drop the value yet.
    #[inline]
    pub fn cancel(&mut self) {
        trace!("signal: {:?}", State::Closed);
        self.signal(State::Closed)
    }

    /// Signal to the `Giver` that a value is wanted.
    #[inline]
    pub fn want(&mut self) {
        trace!("signal: {:?}", State::Want);
        self.signal(State::Want)
    }

    #[inline]
    fn signal(&mut self, state: State) {
        let old_state = self.inner.state.swap(state.into(), Ordering::SeqCst).into();
        match old_state {
            State::Idle | State::Want | State::Closed => (),
            State::Give => {
                loop {
                    if let Some(mut locked) = self.inner.waker.try_lock() {
                        if let Some(waker) = locked.take() {
                            trace!("signal found waiting giver, notifying");
                            waker.wake();
                        }
                        return;
                    } else {
                        // if we couldn't take the lock, then a Giver has it.
                        // The *ONLY* reason is because it is in the process of parking.
                        //
                        // We need to loop and take the lock so we can notify this task.
                    }
                }
            },
        }
    }
}

impl Drop for Taker {
    #[inline]
    fn drop(&mut self) {
        self.signal(State::Closed);
    }
}

impl fmt::Debug for Taker {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Taker")
            .field("state", &self.inner.state())
            .finish()
    }
}

// ===== impl Closed ======

impl fmt::Debug for Closed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Closed")
            .finish()
    }
}

// ===== impl Inner ======

impl Inner {
    #[inline]
    fn state(&self) -> State {
        self.state.load(Ordering::SeqCst).into()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use futures::{Async, Stream};
    use futures::future::poll_fn;
    use futures::executor::block_on;
    use futures::channel::{mpsc, oneshot};
    use super::*;

    #[test]
    fn want_ready() {
        let (mut gv, mut tk) = new();
        tk.want();
        assert!(block_on(poll_fn(|cx| gv.poll_want(cx))).is_ok());
    }

    #[test]
    fn want_notify() {
        let (mut gv, mut tk) = new();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            tk.want();
            // use a oneshot to keep this thread alive
            // until other thread was notified of want
            block_on(rx).expect("rx");
        });

        block_on(poll_fn(move |cx| {
            gv.poll_want(cx)
        })).expect("wait");
        tx.send(()).expect("tx");
    }

    #[test]
    fn cancel() {
        // explicit
        let (mut gv, mut tk) = new();

        assert!(!gv.is_canceled());

        tk.cancel();

        assert!(gv.is_canceled());
        assert!(block_on(poll_fn(|cx| gv.poll_want(cx))).is_err());

        // implicit
        let (mut gv, tk) = new();

        assert!(!gv.is_canceled());

        drop(tk);

        assert!(gv.is_canceled());
        assert!(block_on(poll_fn(|cx| gv.poll_want(cx))).is_err());

        // notifies
        let (mut gv, tk) = new();

        thread::spawn(move || {
            let _tk = tk;
            // and dropped
        });

        block_on(poll_fn(move |cx| {
            gv.poll_want(cx)
        })).expect_err("wait");
    }

    #[test]
    fn stress() {
        let nthreads = 5;
        let nwants = 100;

        for _ in 0..nthreads {
            let (mut gv, mut tk) = new();
            let (mut tx, mut rx) = mpsc::channel(0);

            // rx thread
            thread::spawn(move || {
                let mut cnt = 0;
                block_on(poll_fn(move |cx| {
                    while cnt < nwants {
                        let n = match rx.poll_next(cx).expect("rx poll") {
                            Async::Ready(n) => n.expect("rx opt"),
                            Async::Pending => {
                                tk.want();
                                return Ok(Async::Pending);
                            },
                        };
                        assert_eq!(cnt, n);
                        cnt += 1;
                    }
                    Ok::<_, ()>(Async::Ready(()))
                })).expect("rx wait");
            });

            // tx thread
            thread::spawn(move || {
                let mut cnt = 0;
                let nsent = block_on(poll_fn(move |cx| {
                    loop {
                        while let Ok(()) = tx.try_send(cnt) {
                            cnt += 1;
                        }
                        match gv.poll_want(cx) {
                            Ok(Async::Ready(_)) => (),
                            Ok(Async::Pending) => return Ok::<_, ()>(Async::Pending),
                            Err(_) => return Ok(Async::Ready(cnt)),
                        }
                    }
                })).expect("tx wait");

                assert_eq!(nsent, nwants);
            }).join().expect("thread join");
        }
    }
}
