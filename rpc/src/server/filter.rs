// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use crate::{
    server::{self, Channel},
    util::Compact,
};
use fnv::FnvHashMap;
use futures::{
    channel::mpsc,
    future::AbortRegistration,
    prelude::*,
    ready,
    stream::Fuse,
    task::{Context, Poll},
};
use log::{debug, info, trace};
use pin_utils::{unsafe_pinned, unsafe_unpinned};
use raii_counter::{Counter, WeakCounter};
use std::sync::{Arc, Weak};
use std::{
    collections::hash_map::Entry, convert::TryInto, fmt, hash::Hash, marker::Unpin, pin::Pin,
};

/// A single-threaded filter that drops channels based on per-key limits.
#[derive(Debug)]
pub struct ChannelFilter<S, K, F>
where
    K: Eq + Hash,
{
    listener: Fuse<S>,
    channels_per_key: u32,
    dropped_keys: mpsc::UnboundedReceiver<K>,
    dropped_keys_tx: mpsc::UnboundedSender<K>,
    key_counts: FnvHashMap<K, TrackerPrototype<K>>,
    keymaker: F,
}

/// A channel that is tracked by a ChannelFilter.
#[derive(Debug)]
pub struct TrackedChannel<C, K> {
    inner: C,
    tracker: Tracker<K>,
}

impl<C, K> TrackedChannel<C, K> {
    unsafe_pinned!(inner: C);
}

#[derive(Clone, Debug)]
struct Tracker<K> {
    key: Option<Arc<K>>,
    counter: Counter,
    dropped_keys: mpsc::UnboundedSender<K>,
}

impl<K> Drop for Tracker<K> {
    fn drop(&mut self) {
        if self.counter.count() <= 1 {
            // Don't care if the listener is dropped.
            match Arc::try_unwrap(self.key.take().unwrap()) {
                Ok(key) => {
                    let _ = self.dropped_keys.unbounded_send(key);
                }
                _ => unreachable!(),
            }
        }
    }
}

#[derive(Clone, Debug)]
struct TrackerPrototype<K> {
    key: Weak<K>,
    counter: WeakCounter,
    dropped_keys: mpsc::UnboundedSender<K>,
}

impl<C, K> Stream for TrackedChannel<C, K>
where
    C: Stream,
{
    type Item = <C as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.channel().poll_next(cx)
    }
}

impl<C, I, K> Sink<I> for TrackedChannel<C, K>
where
    C: Sink<I>,
{
    type Error = C::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.channel().poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: I) -> Result<(), Self::Error> {
        self.channel().start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.channel().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.channel().poll_close(cx)
    }
}

impl<C, K> AsRef<C> for TrackedChannel<C, K> {
    fn as_ref(&self) -> &C {
        &self.inner
    }
}

impl<C, K> Channel for TrackedChannel<C, K>
where
    C: Channel,
{
    type Req = C::Req;
    type Resp = C::Resp;

    fn config(&self) -> &server::Config {
        self.inner.config()
    }

    fn in_flight_requests(self: Pin<&mut Self>) -> usize {
        self.inner().in_flight_requests()
    }

    fn start_request(self: Pin<&mut Self>, request_id: u64) -> AbortRegistration {
        self.inner().start_request(request_id)
    }
}

impl<C, K> TrackedChannel<C, K> {
    /// Returns the inner channel.
    pub fn get_ref(&self) -> &C {
        &self.inner
    }

    /// Returns the pinned inner channel.
    fn channel<'a>(self: Pin<&'a mut Self>) -> Pin<&'a mut C> {
        self.inner()
    }
}

impl<S, K, F> ChannelFilter<S, K, F>
where
    K: fmt::Display + Eq + Hash + Clone,
{
    unsafe_pinned!(listener: Fuse<S>);
    unsafe_pinned!(dropped_keys: mpsc::UnboundedReceiver<K>);
    unsafe_pinned!(dropped_keys_tx: mpsc::UnboundedSender<K>);
    unsafe_unpinned!(key_counts: FnvHashMap<K, TrackerPrototype<K>>);
    unsafe_unpinned!(channels_per_key: u32);
    unsafe_unpinned!(keymaker: F);
}

impl<S, K, F> ChannelFilter<S, K, F>
where
    K: Eq + Hash,
    S: Stream,
    F: Fn(&S::Item) -> K,
{
    /// Sheds new channels to stay under configured limits.
    pub(crate) fn new(listener: S, channels_per_key: u32, keymaker: F) -> Self {
        let (dropped_keys_tx, dropped_keys) = mpsc::unbounded();
        ChannelFilter {
            listener: listener.fuse(),
            channels_per_key,
            dropped_keys,
            dropped_keys_tx,
            key_counts: FnvHashMap::default(),
            keymaker,
        }
    }
}

impl<S, K, F> ChannelFilter<S, K, F>
where
    S: Stream,
    K: fmt::Display + Eq + Hash + Clone + Unpin,
    F: Fn(&S::Item) -> K,
{
    fn handle_new_channel(
        mut self: Pin<&mut Self>,
        stream: S::Item,
    ) -> Result<TrackedChannel<S::Item, K>, K> {
        let key = self.as_mut().keymaker()(&stream);
        let tracker = self.as_mut().increment_channels_for_key(key.clone())?;

        trace!(
            "[{}] Opening channel ({}/{}) channels for key.",
            key,
            tracker.counter.count(),
            self.as_mut().channels_per_key()
        );

        Ok(TrackedChannel {
            tracker,
            inner: stream,
        })
    }

    fn increment_channels_for_key(mut self: Pin<&mut Self>, key: K) -> Result<Tracker<K>, K> {
        let channels_per_key = self.channels_per_key;
        let dropped_keys = self.dropped_keys_tx.clone();
        let key_counts = &mut self.as_mut().key_counts();
        match key_counts.entry(key.clone()) {
            Entry::Vacant(vacant) => {
                let key = Arc::new(key);
                let counter = WeakCounter::new();

                vacant.insert(TrackerPrototype {
                    key: Arc::downgrade(&key),
                    counter: counter.clone(),
                    dropped_keys: dropped_keys.clone(),
                });
                Ok(Tracker {
                    key: Some(key),
                    counter: counter.upgrade(),
                    dropped_keys,
                })
            }
            Entry::Occupied(o) => {
                let count = o.get().counter.count();
                if count >= channels_per_key.try_into().unwrap() {
                    info!(
                        "[{}] Opened max channels from key ({}/{}).",
                        key, count, channels_per_key
                    );
                    Err(key)
                } else {
                    let TrackerPrototype {
                        key,
                        counter,
                        dropped_keys,
                    } = o.get().clone();
                    Ok(Tracker {
                        counter: counter.upgrade(),
                        key: Some(key.upgrade().unwrap()),
                        dropped_keys,
                    })
                }
            }
        }
    }

    fn poll_listener(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<TrackedChannel<S::Item, K>, K>>> {
        match ready!(self.as_mut().listener().poll_next_unpin(cx)) {
            Some(codec) => Poll::Ready(Some(self.handle_new_channel(codec))),
            None => Poll::Ready(None),
        }
    }

    fn poll_closed_channels(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match ready!(self.as_mut().dropped_keys().poll_next_unpin(cx)) {
            Some(key) => {
                debug!("All channels dropped for key [{}]", key);
                self.as_mut().key_counts().remove(&key);
                self.as_mut().key_counts().compact(0.1);
                Poll::Ready(())
            }
            None => unreachable!("Holding a copy of closed_channels and didn't close it."),
        }
    }
}

impl<S, K, F> Stream for ChannelFilter<S, K, F>
where
    S: Stream,
    K: fmt::Display + Eq + Hash + Clone + Unpin,
    F: Fn(&S::Item) -> K,
{
    type Item = TrackedChannel<S::Item, K>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<TrackedChannel<S::Item, K>>> {
        loop {
            match (
                self.as_mut().poll_listener(cx),
                self.as_mut().poll_closed_channels(cx),
            ) {
                (Poll::Ready(Some(Ok(channel))), _) => {
                    return Poll::Ready(Some(channel));
                }
                (Poll::Ready(Some(Err(_))), _) => {
                    continue;
                }
                (_, Poll::Ready(())) => continue,
                (Poll::Pending, Poll::Pending) => return Poll::Pending,
                (Poll::Ready(None), Poll::Pending) => {
                    trace!("Shutting down listener.");
                    return Poll::Ready(None);
                }
            }
        }
    }
}

#[cfg(test)]
fn ctx() -> Context<'static> {
    use futures_test::task::noop_waker_ref;

    Context::from_waker(&noop_waker_ref())
}

#[test]
fn tracker_drop() {
    use assert_matches::assert_matches;
    use raii_counter::Counter;

    let (tx, mut rx) = mpsc::unbounded();
    Tracker {
        key: Some(Arc::new(1)),
        counter: Counter::new(),
        dropped_keys: tx,
    };
    assert_matches!(rx.try_next(), Ok(Some(1)));
}

#[test]
fn tracked_channel_stream() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;
    use raii_counter::Counter;

    let (chan_tx, chan) = mpsc::unbounded();
    let (dropped_keys, _) = mpsc::unbounded();
    let channel = TrackedChannel {
        inner: chan,
        tracker: Tracker {
            key: Some(Arc::new(1)),
            counter: Counter::new(),
            dropped_keys,
        },
    };

    chan_tx.unbounded_send("test").unwrap();
    pin_mut!(channel);
    assert_matches!(channel.poll_next(&mut ctx()), Poll::Ready(Some("test")));
}

#[test]
fn tracked_channel_sink() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;
    use raii_counter::Counter;

    let (chan, mut chan_rx) = mpsc::unbounded();
    let (dropped_keys, _) = mpsc::unbounded();
    let channel = TrackedChannel {
        inner: chan,
        tracker: Tracker {
            key: Some(Arc::new(1)),
            counter: Counter::new(),
            dropped_keys,
        },
    };

    pin_mut!(channel);
    assert_matches!(channel.as_mut().poll_ready(&mut ctx()), Poll::Ready(Ok(())));
    assert_matches!(channel.as_mut().start_send("test"), Ok(()));
    assert_matches!(channel.as_mut().poll_flush(&mut ctx()), Poll::Ready(Ok(())));
    assert_matches!(chan_rx.try_next(), Ok(Some("test")));
}

#[test]
fn channel_filter_increment_channels_for_key() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;

    struct TestChannel {
        key: &'static str,
    }
    let (_, listener) = mpsc::unbounded();
    let filter = ChannelFilter::new(listener, 2, |chan: &TestChannel| chan.key);
    pin_mut!(filter);
    let tracker1 = filter.as_mut().increment_channels_for_key("key").unwrap();
    assert_eq!(tracker1.counter.count(), 1);
    let tracker2 = filter.as_mut().increment_channels_for_key("key").unwrap();
    assert_eq!(tracker1.counter.count(), 2);
    assert_matches!(filter.increment_channels_for_key("key"), Err("key"));
    drop(tracker2);
    assert_eq!(tracker1.counter.count(), 1);
}

#[test]
fn channel_filter_handle_new_channel() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;

    #[derive(Debug)]
    struct TestChannel {
        key: &'static str,
    }
    let (_, listener) = mpsc::unbounded();
    let filter = ChannelFilter::new(listener, 2, |chan: &TestChannel| chan.key);
    pin_mut!(filter);
    let channel1 = filter
        .as_mut()
        .handle_new_channel(TestChannel { key: "key" })
        .unwrap();
    assert_eq!(channel1.tracker.counter.count(), 1);

    let channel2 = filter
        .as_mut()
        .handle_new_channel(TestChannel { key: "key" })
        .unwrap();
    assert_eq!(channel1.tracker.counter.count(), 2);

    assert_matches!(
        filter.handle_new_channel(TestChannel { key: "key" }),
        Err("key")
    );
    drop(channel2);
    assert_eq!(channel1.tracker.counter.count(), 1);
}

#[test]
fn channel_filter_poll_listener() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;

    #[derive(Debug)]
    struct TestChannel {
        key: &'static str,
    }
    let (new_channels, listener) = mpsc::unbounded();
    let filter = ChannelFilter::new(listener, 2, |chan: &TestChannel| chan.key);
    pin_mut!(filter);

    new_channels
        .unbounded_send(TestChannel { key: "key" })
        .unwrap();
    let channel1 =
        assert_matches!(filter.as_mut().poll_listener(&mut ctx()), Poll::Ready(Some(Ok(c))) => c);
    assert_eq!(channel1.tracker.counter.count(), 1);

    new_channels
        .unbounded_send(TestChannel { key: "key" })
        .unwrap();
    let _channel2 =
        assert_matches!(filter.as_mut().poll_listener(&mut ctx()), Poll::Ready(Some(Ok(c))) => c);
    assert_eq!(channel1.tracker.counter.count(), 2);

    new_channels
        .unbounded_send(TestChannel { key: "key" })
        .unwrap();
    let key =
        assert_matches!(filter.as_mut().poll_listener(&mut ctx()), Poll::Ready(Some(Err(k))) => k);
    assert_eq!(key, "key");
    assert_eq!(channel1.tracker.counter.count(), 2);
}

#[test]
fn channel_filter_poll_closed_channels() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;

    #[derive(Debug)]
    struct TestChannel {
        key: &'static str,
    }
    let (new_channels, listener) = mpsc::unbounded();
    let filter = ChannelFilter::new(listener, 2, |chan: &TestChannel| chan.key);
    pin_mut!(filter);

    new_channels
        .unbounded_send(TestChannel { key: "key" })
        .unwrap();
    let channel =
        assert_matches!(filter.as_mut().poll_listener(&mut ctx()), Poll::Ready(Some(Ok(c))) => c);
    assert_eq!(filter.key_counts.len(), 1);

    drop(channel);
    assert_matches!(
        filter.as_mut().poll_closed_channels(&mut ctx()),
        Poll::Ready(())
    );
    assert!(filter.key_counts.is_empty());
}

#[test]
fn channel_filter_stream() {
    use assert_matches::assert_matches;
    use pin_utils::pin_mut;

    #[derive(Debug)]
    struct TestChannel {
        key: &'static str,
    }
    let (new_channels, listener) = mpsc::unbounded();
    let filter = ChannelFilter::new(listener, 2, |chan: &TestChannel| chan.key);
    pin_mut!(filter);

    new_channels
        .unbounded_send(TestChannel { key: "key" })
        .unwrap();
    let channel = assert_matches!(filter.as_mut().poll_next(&mut ctx()), Poll::Ready(Some(c)) => c);
    assert_eq!(filter.key_counts.len(), 1);

    drop(channel);
    assert_matches!(filter.as_mut().poll_next(&mut ctx()), Poll::Pending);
    assert!(filter.key_counts.is_empty());
}
