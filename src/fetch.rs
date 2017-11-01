use std::io::{self, Write};
use std::cmp::min;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::str::from_utf8;
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};

use abstract_ns::Address;
use futures::{Sink, Async, Stream};
use futures::future::{Future, join_all, ok, FutureResult};
use tokio_core::net::TcpStream;
use tokio_core::reactor::Timeout;
use tk_easyloop::{spawn, handle, timeout_at};
use tk_http::{Version, Status};
use tk_http::client::{Proto, Config, Error, Codec};
use tk_http::client::{Encoder, EncoderDone, Head, RecvMode};
use url::Url;
use ns_router::Router;


#[cfg(feature="tls_native")] use native_tls::TlsConnector;
#[cfg(feature="tls_native")] use tokio_tls::TlsConnectorExt;
#[cfg(feature="tls_rustls")] use rustls::ClientConfig;
#[cfg(feature="tls_rustls")] use tokio_rustls::ClientConfigExt;

use forwarder::Forwarder;


#[derive(Debug)]
struct State {
    offset: u64,
    eof: u32,
    last_line: Vec<u8>,
    last_request: Instant,
}

#[derive(Debug)]
struct Cursor {
    url: Arc<Url>,
    state: Option<State>,
}

struct Requests {
    cursors: VecDeque<Arc<Mutex<Cursor>>>,
    timeout: Timeout,
}

#[derive(Debug)]
pub struct Request {
    cursor: Arc<Mutex<Cursor>>,
    range: Option<(u64, u64, u64)>,
}

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
    let cfg = Config::new()
        .keep_alive_timeout(Duration::new(25, 0))
        .done();
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

fn request(cur: &Arc<Mutex<Cursor>>) -> Result<Request, Instant> {
    let intr = cur.lock().unwrap();
    match intr.state {
        None => return Ok(Request {
            cursor: cur.clone(),
            range: None,
        }),
        Some(ref state) => {
            if state.eof == 0 {
                return Ok(Request {
                    cursor: cur.clone(),
                    range: None,
                });
            }
            let next = state.last_request +
                Duration::from_millis(
                    min(100 * (2 << min(state.eof, 10)), 15000));
            if next < Instant::now() {
                return Ok(Request {
                    cursor: cur.clone(),
                    range: None,
                });
            }
            return Err(next);
        }
    }
}

impl Requests {
    fn new(urls: Vec<Arc<Url>>) -> Requests {
        Requests {
            cursors: urls.into_iter().map(|u| Arc::new(Mutex::new(Cursor {
                url: u,
                state: None,
            }))).collect(),
            timeout: timeout_at(Instant::now()),
        }
    }
}

impl Stream for Requests {
    type Item = Request;
    type Error = ();
    fn poll(&mut self) -> Result<Async<Option<Request>>, ()> {
        loop {
            match self.timeout.poll().unwrap() {
                Async::Ready(()) => {}
                Async::NotReady => return Ok(Async::NotReady),
            }
            let mut min_time = Instant::now() + Duration::new(7, 0);
            for _ in 0..self.cursors.len() {
                let cur = self.cursors.pop_front().unwrap();
                let req = request(&cur);
                self.cursors.push_back(cur);
                match req {
                    Ok(req) => return Ok(Async::Ready(Some(req))),
                    Err(time) if min_time > time => min_time = time,
                    Err(_) => {}
                }
            }
            self.timeout = timeout_at(min_time);
        }
    }
}

impl<S> Codec<S> for Request {
    type Future = FutureResult<EncoderDone<S>, Error>;
    fn start_write(&mut self, mut e: Encoder<S>) -> Self::Future {
        let cur = self.cursor.lock().unwrap();
        e.request_line("GET", cur.url.path(), Version::Http11);
        cur.url.host_str().map(|x| {
            e.add_header("Host", x).unwrap();
        });
        match cur.state {
            Some(State { offset, .. }) => {
                e.format_header("Range",
                    format_args!("bytes={}-{}",
                        offset-1, offset+65535)).unwrap();
            }
            None => {
                e.add_header("Range", "bytes=-4096").unwrap();
            }
        }
        e.done_headers().unwrap();
        ok(e.done())
    }
    fn headers_received(&mut self, headers: &Head) -> Result<RecvMode, Error> {
        let status = headers.status();
        // TODO(tailhook) better error
        assert_eq!(Some(Status::PartialContent), status);
        for (name, value) in headers.headers() {
            if name == "Content-Range" {
                let str_value = from_utf8(value)
                    .expect("valid content-range header");
                if !str_value.starts_with("bytes ") {
                    panic!("invalid content-range header");
                }
                let slash = str_value.find("/")
                    .expect("valid content-range header");
                let dash = str_value[..slash].find("-")
                    .expect("valid content-range header");
                let from = str_value[6..dash].parse::<u64>()
                    .expect("valid content-range header");
                let to = str_value[dash+1..slash].parse::<u64>()
                    .expect("valid content-range header");
                let total = str_value[slash+1..].parse::<u64>()
                    .expect("valid content-range header");
                self.range = Some((from, to, total));
            }
        }
        Ok(RecvMode::buffered(65536))
    }
    fn data_received(&mut self, data: &[u8], end: bool)
        -> Result<Async<usize>, Error>
    {
        assert!(end);
        let consumed = data.len();
        let (from, to, total) = self.range.unwrap();
        let mut cur = self.cursor.lock().unwrap();
        let (pos, eof, mut last_line) = match cur.state.take() {
            Some(state) => (Some(state.offset), state.eof, state.last_line),
            None => (None, 0, b"".to_vec()),
        };
        let data = if pos.is_some() {
            if pos != Some(from+1) {
                last_line.clear();
                println!("[.. skipped ..]");
                &data
            } else {
                &data[1..]
            }
        } else {
            &data
        };
        let (last_line, end) = match data.iter().rposition(|&x| x == b'\n') {
            Some(end) => (data[end+1..].to_vec(), end+1),
            None => ({last_line.extend(data); last_line}, 0)
        };
        cur.state = Some(State {
            eof: if to+1 == total { eof + 1 } else { 0 },
            offset: to+1,
            last_line: last_line,
            last_request: Instant::now(),
        });
        io::stdout().write_all(&data[..end]).unwrap();
        io::stdout().flush().unwrap();
        Ok(Async::Ready(consumed))
    }
}

fn spawn_updater<S: Sink<SinkItem=Request>>(sink: S, urls: Vec<Arc<Url>>)
    where S::SinkError: fmt::Display,
          S: 'static,
{
    spawn(Forwarder::new(Requests::new(urls), sink));
}
