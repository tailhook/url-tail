use std::fmt;

use futures::sink::Sink;
use futures::stream::{Stream, Fuse};
use futures::future::Future;
use futures::{Async, AsyncSink};


// This is similar to `futures::stream::Forward` but also calls poll_complete
// on wakeups. This is important to keep connection pool up to date when
// no new requests are coming in.
pub struct Forwarder<T: Stream, K: Sink<SinkItem=T::Item>> {
    source: Fuse<T>,
    sink: K,
    buffered: Option<T::Item>,
}

impl<T: Stream<Error=()>, K: Sink<SinkItem=T::Item>> Forwarder<T, K>
    where K::SinkError: fmt::Display,
{
    pub fn new(source: T, dest: K) -> Forwarder<T, K> {
        Forwarder {
            source: source.fuse(),
            sink: dest,
            buffered: None,
        }
    }
}

impl<T: Stream<Error=()>, K: Sink<SinkItem=T::Item>> Future for Forwarder<T, K>
    where K::SinkError: fmt::Display,
{
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<()>, ()> {
        // If we've got an item buffered already, we need to write it to the
        // sink before we can do anything else
        while let Some(item) = self.buffered.take() {
            let res = self.sink.start_send(item)
                .map_err(|e| error!("Sink error: {}. Stopped.", e))?;
            match res {
                AsyncSink::NotReady(item) => {
                    self.buffered = Some(item);
                    return Ok(Async::NotReady);
                }
                AsyncSink::Ready => {}
            }
        }

        loop {
            let res = self.source.poll()
                .map_err(|()| error!("Pool input aborted. Stopping."))?;
            match res {
                Async::Ready(Some(item)) => {
                    let res = self.sink.start_send(item)
                        .map_err(|e| error!("Pool output error: {}. \
                                             Stopping pool.", e))?;
                    match res {
                        AsyncSink::NotReady(item) => {
                            self.buffered = Some(item);
                            break;
                        }
                        AsyncSink::Ready => {}
                    }
                }
                Async::Ready(None) => {
                    let res = self.sink.poll_complete()
                        .map_err(|e| error!("Pool output error: {}. \
                            Stopping pool.", e))?;
                    match res {
                        Async::Ready(()) => return Ok(Async::Ready(())),
                        Async::NotReady => return Ok(Async::NotReady),
                    }
                }
                Async::NotReady => {
                    self.sink.poll_complete()
                        .map_err(|e| error!("Pool output error: {}. \
                            Stopping pool.", e))?;
                    break;
                }
            }
        }
        Ok(Async::NotReady)
    }
}
