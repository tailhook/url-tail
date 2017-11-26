extern crate abstract_ns;
extern crate argparse;
extern crate env_logger;
extern crate futures;
extern crate futures_cpupool;
extern crate ns_router;
extern crate ns_std_threaded;
extern crate tk_easyloop;
extern crate tk_http;
extern crate tokio_core;
extern crate url;
#[macro_use] extern crate log;
#[cfg(feature="tls_native")] extern crate native_tls;
#[cfg(feature="tls_native")] extern crate tokio_tls;
#[cfg(feature="tls_rustls")] extern crate rustls;
#[cfg(feature="tls_rustls")] extern crate tokio_rustls;
#[cfg(feature="tls_rustls")] extern crate webpki_roots;

mod fetch;

use futures::Future;
use std::env;
use std::process::exit;
use std::sync::Arc;
use std::collections::HashMap;

use tokio_core::reactor::Handle;

pub struct Options {
    pub urls: Vec<url::Url>,
}


fn main() {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "warn");
    }
    env_logger::init().expect("logging init");

    let options = options();
    let mut keep_resolver = None;
    tk_easyloop::run(|| {
        let resolver = resolver(&tk_easyloop::handle());
        keep_resolver = Some(resolver.clone());
        let mut http = HashMap::new();
        let mut https = HashMap::new();
        for url in options.urls.iter().cloned().map(Arc::new) {
            let host = format!("{}:{}",
                url.host_str().expect("http url has host"),
                url.port_or_known_default()
                    .expect("http and https are supported"));
            match url.scheme() {
                "http" => {
                    http.entry(host)
                    .or_insert_with(Vec::new)
                    .push(url.clone());
                }
                "https" => {
                    https.entry(host)
                    .or_insert_with(Vec::new)
                    .push(url.clone());
                }
                s => {
                    eprintln!("Url {} has unknown scheme {:?}, \
                        but only http:// and https:// are supported", s, url);
                }
            }
        }
        fetch::http(&resolver, http).join(fetch::https(&resolver, https))
    }).map(|(_, _)| exit(1)).unwrap_or_else(|()| exit(1));
}

pub fn resolver(h: &Handle) -> ns_router::Router {
    use ns_router::{Router, Config};
    use ns_std_threaded::ThreadedResolver;
    use abstract_ns::{Resolve, HostResolve};
    use futures_cpupool::CpuPool;

    return Router::from_config(&Config::new()
        .set_fallthrough(ThreadedResolver::use_pool(CpuPool::new(1))
            .null_service_resolver()
            .frozen_subscriber())
        .done(), h)
}

pub fn options() -> Options {
    use argparse::*;

    let mut options = Options {
        urls: Vec::new(),
    };
    {
        let mut ap = ArgumentParser::new();
        ap.refer(&mut options.urls)
            .add_argument("url", Collect, "URL to follow");
        ap.parse_args_or_exit();
    }
    return options;
}
