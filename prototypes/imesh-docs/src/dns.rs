//! Name resolution through the operating system.
//!
//! iroh's built-in resolver talks to nameservers directly. On some networks, for example a
//! Windows VM behind a NAT DNS proxy, that fails while the OS resolver works. Hosts go
//! through the OS; TXT records, which the OS cannot look up, go to iroh's resolver.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use iroh_dns::dns::{BoxIter, DnsError, DnsResolver, Resolver, TxtRecordData};
use n0_error::AnyError;
use n0_future::boxed::BoxFuture;

#[derive(Debug, Clone)]
pub struct OsResolver {
    txt: DnsResolver,
}

impl OsResolver {
    pub fn resolver() -> DnsResolver {
        DnsResolver::custom(OsResolver { txt: DnsResolver::new() })
    }
}

async fn lookup(host: String) -> Result<Vec<IpAddr>, DnsError> {
    let addrs = tokio::net::lookup_host((host.as_str(), 0))
        .await
        .map_err(|e| DnsError::from(AnyError::from_std(e)))?;
    Ok(addrs.map(|a| a.ip()).collect())
}

fn none() -> DnsError {
    DnsError::from(AnyError::from_std(std::io::Error::other("no address of this family")))
}

impl Resolver for OsResolver {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        Box::pin(async move {
            let v: Vec<_> = lookup(host).await?.into_iter().filter_map(|a| match a {
                IpAddr::V4(a) => Some(a),
                IpAddr::V6(_) => None,
            }).collect();
            if v.is_empty() {
                return Err(none());
            }
            Ok(Box::new(v.into_iter()) as BoxIter<_>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        Box::pin(async move {
            let v: Vec<_> = lookup(host).await?.into_iter().filter_map(|a| match a {
                IpAddr::V6(a) => Some(a),
                IpAddr::V4(_) => None,
            }).collect();
            if v.is_empty() {
                return Err(none());
            }
            Ok(Box::new(v.into_iter()) as BoxIter<_>)
        })
    }

    fn lookup_txt(&self, host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        let txt = self.txt.clone();
        Box::pin(async move {
            let r: Vec<_> = txt.lookup_txt(host, Duration::from_secs(5)).await?.collect();
            Ok(Box::new(r.into_iter()) as BoxIter<_>)
        })
    }

    fn clear_cache(&self) {
        self.txt.clear_cache();
    }

    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(OsResolver { txt: DnsResolver::new() })
    }
}
