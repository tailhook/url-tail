use std::fmt;
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use abstract_ns::Address;
use futures::{Sink, Async, Stream};
use futures::future::{Future, join_all, ok, FutureResult};
use tokio_core::net::TcpStream;
use tk_easyloop::{spawn, handle};
use tk_http::{Version, Status};
use tk_http::client::{Proto, Config, Error, Codec};
use tk_http::client::{Encoder, EncoderDone, Head, RecvMode};
use tk_http::client::buffered::Buffered;
use url::Url;
use ns_router::Router;

#[cfg(feature="tls_native")] use native_tls::TlsConnector;
#[cfg(feature="tls_native")] use tokio_tls::TlsConnectorExt;
#[cfg(feature="tls_rustls")] use rustls::ClientConfig;
#[cfg(feature="tls_rustls")] use tokio_rustls::ClientConfigExt;

pub fn group_addrs(vec: Vec<(Address, Vec<Arc<Url>>)>)
    -> HashMap<SocketAddr, Vec<Arc<Url>>>
{
    let mut urls_by_ip = HashMap::new();
    for (addr, urls) in vec {
        for sa in addr.addresses_at(0) {
            let set = urls_by_ip.entry(sa)
                .or_insert_with(HashSet::new);
            for url in &urls {
                set.insert(url.clone());
            }
        }
    }
    let mut ordered = urls_by_ip.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|&(_, y)| y.len());
    let mut active_ips = HashMap::new();
    let mut visited_urls = HashSet::new();
    for (ip, urls) in ordered {
        let urls = urls.difference(&visited_urls).cloned().collect::<Vec<_>>();
        if urls.len() == 0 {
            continue;
        }
        visited_urls.extend(urls.iter().cloned());
        active_ips.insert(*ip, urls);
    }
    return active_ips;
}

pub fn http(resolver: &Router, urls_by_host: HashMap<String, Vec<Arc<Url>>>) {
    let resolver = resolver.clone();
    let cfg = Config::new().done();
    spawn(
        join_all(urls_by_host.into_iter().map(move |(host, list)| {
            resolver.resolve_auto(&host, 80).map(|ips| (ips, list))
        }))
        .map_err(|e| error!("Error resolving: {}", e))
        .map(group_addrs)
        .map(move |map| {
            for (ip, urls) in map {
                let cfg = cfg.clone();
                spawn(
                    TcpStream::connect(&ip, &handle()).map(move |sock| {
                        spawn_updater(Proto::new(sock, &handle(), &cfg), urls)
                    }).map_err(move |e| {
                        error!("Error connecting to {}: {}", ip, e);
                    }));
            }
        }));
}
#[cfg(not(any(feature="tls_native", feature="tls_rustls")))]
pub fn https(_resolver: &Router, urls_by_host: HashMap<String, Vec<Arc<Url>>>)
{
    if urls_by_host.len() > 0 {
        eprintln!("Compiled without TLS support");
    }
}

#[cfg(feature="tls_native")]
pub fn https(resolver: &Router, urls_by_host: HashMap<String, Vec<Arc<Url>>>) {
    use std::io;

    if urls_by_host.len() == 0 {
        return;
    }
    let resolver = resolver.clone();
    let cfg = Config::new().done();
    let cx = TlsConnector::builder().expect("tls builder can be created works")
        .build().expect("tls builder works");
    spawn(
        join_all(urls_by_host.into_iter().map(move |(host, list)| {
            resolver.resolve_auto(&host, 80).map(|addr| (host, addr, list))
        }))
        .map_err(|e| error!("Error resolving: {}", e))
        .map(move |map| {
            for (host, addr, urls) in map {
                let ip = addr.pick_one().expect("no ips");
                let cfg = cfg.clone();
                let cx = cx.clone();
                spawn(
                    TcpStream::connect(&ip, &handle())
                    .and_then(move |sock| {
                        cx.connect_async(&host, sock).map_err(|e| {
                            io::Error::new(io::ErrorKind::Other, e)
                        })
                    })
                    .map(move |sock| {
                        spawn_updater(Proto::new(sock, &handle(), &cfg), urls)
                    }).map_err(move |e| {
                        error!("Error connecting to {}: {}", ip, e);
                    }));
            }
        }));
}

#[cfg(feature="tls_rustls")]
pub fn https(resolver: &Router, urls_by_host: HashMap<String, Vec<Arc<Url>>>) {
    use std::io::BufReader;
    use std::fs::File;

    if urls_by_host.len() == 0 {
        return;
    }
    let resolver = resolver.clone();
    let tls = Arc::new({
        let mut cfg = ClientConfig::new();
        let mut pem = BufReader::new(
            File::open("/etc/ssl/certs/ca-certificates.crt")
            .expect("certificates exist"));
        cfg.root_store.add_pem_file(&mut pem).unwrap();
        cfg
    });
    let cfg = Config::new().done();
    spawn(
        join_all(urls_by_host.into_iter().map(move |(host, list)| {
            resolver.resolve_auto(&host, 80).map(|addr| (host, addr, list))
        }))
        .map_err(|e| error!("Error resolving: {}", e))
        .map(move |map| {
            for (host, addr, urls) in map {
                let ip = addr.pick_one().expect("no ips");
                let cfg = cfg.clone();
                let tls = tls.clone();
                spawn(
                    TcpStream::connect(&ip, &handle())
                    .and_then(move |sock| tls.connect_async(&host, sock))
                    .map(move |sock| {
                        spawn_updater(Proto::new(sock, &handle(), &cfg), urls)
                    }).map_err(move |e| {
                        error!("Error connecting to {}: {}", ip, e);
                    }));
            }
        }));
}

struct Cursor {
    offset: Option<u64>,
}

struct Requests {

}

impl Requests {
    fn new(urls: Vec<Arc<Url>>) -> Requests {
        unimplemented!();
    }
}

pub struct Request {
    url: Arc<Url>,
    cursor: Arc<Mutex<Cursor>>,
}

impl Stream for Requests {
    type Item = Request;
    type Error = ();
    fn poll(&mut self) -> Result<Async<Option<Request>>, ()> {
        unimplemented!();
    }
}

impl<S> Codec<S> for Request {
    type Future = FutureResult<EncoderDone<S>, Error>;
    fn start_write(&mut self, mut e: Encoder<S>) -> Self::Future {
        e.request_line("GET", self.url.path(), Version::Http11);
        self.url.host_str().map(|x| {
            e.add_header("Host", x).unwrap();
        });
        e.done_headers().unwrap();
        ok(e.done())
    }
    fn headers_received(&mut self, headers: &Head) -> Result<RecvMode, Error> {
        let status = headers.status();
        // TODO(tailhook) better error
        assert_eq!(Some(Status::PartialContent), status);
        Ok(RecvMode::buffered(65536))
    }
    fn data_received(&mut self, data: &[u8], end: bool)
        -> Result<Async<usize>, Error>
    {
        assert!(end);
        println!("Data {:?}", data);
        Ok(Async::Ready(data.len()))
    }
}

fn spawn_updater<S: Sink<SinkItem=Request>>(sink: S, urls: Vec<Arc<Url>>)
    where S::SinkError: fmt::Display,
          S: 'static,
{
    spawn(sink.sink_map_err(|e| error!("Sink Error: {}", e))
        .send_all(Requests::new(urls))
        .map(|_| {}));
}
