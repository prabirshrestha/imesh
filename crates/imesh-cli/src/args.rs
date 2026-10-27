//! The daemon's flags, and the `IMESH_*` variables that set them. They map to
//! [`imesh::daemon::Config`], and an install writes them to the daemon's env file.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use anyhow::Result;
use imesh::{
    daemon::{self, Access, LocalApi},
    net::{self, Expose},
};
use iroh::RelayUrl;

use crate::paths;

/// `imesh daemon`.
#[derive(Debug, Clone, clap::Args)]
pub struct DaemonArgs {
    /// Where the machine key and state live.
    #[arg(long, env = "IMESH_DATA_DIR", default_value_os_t = paths::data_dir())]
    pub data_dir: PathBuf,
    /// Local API: a unix socket, or a named pipe on Windows.
    #[arg(long, env = "IMESH_SOCKET", default_value_os_t = paths::socket())]
    pub socket: PathBuf,
    /// Let this group use the local API too: the socket's group on Linux, a local group on
    /// Windows. A system install sets it.
    #[arg(long, env = "IMESH_SOCKET_GROUP")]
    pub socket_group: Option<String>,
    /// Machine name. Defaults to the hostname.
    #[arg(long, env = "IMESH_NAME")]
    pub name: Option<String>,
    #[command(flatten)]
    pub settings: Settings,
}

impl DaemonArgs {
    pub fn config(&self) -> daemon::Config {
        daemon::Config {
            data_dir: self.data_dir.clone(),
            name: self.name.clone(),
            labels: self.settings.labels.iter().cloned().collect(),
            net: net::Config {
                proxy: if self.settings.net.devtunnel.is_some() || self.settings.net.expose_relay.is_some() {
                    None
                } else {
                    self.settings.net.proxy.clone().or_else(proxy_from_env)
                },
                ..self.settings.net.config()
            },
        }
    }

    pub fn local_api(&self) -> LocalApi {
        let access = match &self.socket_group {
            Some(group) => Access::Group(group.clone()),
            None => Access::Owner,
        };
        LocalApi { path: self.socket.clone(), access, notify_systemd: true }
    }
}

/// The settings an install writes for the daemon: labels and network.
#[derive(Debug, Clone, Default, clap::Args)]
#[group(skip)]
pub struct Settings {
    /// Labels for the machine, as key=value. Repeat for more.
    #[arg(long = "label", env = "IMESH_LABELS", value_delimiter = ',', value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub net: NetArgs,
}

impl Settings {
    /// The settings as environment variables for the daemon.
    pub fn to_env(&self) -> BTreeMap<String, String> {
        let mut env = self.net.to_env();
        if !self.labels.is_empty() {
            let labels: Vec<_> = self.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            env.insert("IMESH_LABELS".into(), labels.join(","));
        }
        env
    }
}

/// The HTTP proxy from `HTTPS_PROXY` or `HTTP_PROXY`, as curl reads them.
pub fn proxy_from_env() -> Option<url::Url> {
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .and_then(|v| v.parse().ok())
}

/// Parses `key=value`. Labels travel comma-separated in `IMESH_LABELS`, so no comma.
fn parse_label(s: &str) -> Result<(String, String), String> {
    if s.contains(',') {
        return Err(format!("label {s:?} must not contain a comma"));
    }
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("label {s:?} must be key=value"))
}

/// Network settings: see [`net::Config`].
#[derive(Debug, Clone, Default, clap::Args)]
#[group(skip)]
pub struct NetArgs {
    /// Relay servers to use instead of the n0 relays. Repeat or separate with commas.
    #[arg(long = "relay", env = "IMESH_RELAYS", value_delimiter = ',')]
    pub relays: Vec<RelayUrl>,
    /// Bearer token to present to relays that require one.
    #[arg(long, env = "IMESH_RELAY_TOKEN", hide_env_values = true)]
    pub relay_token: Option<String>,
    /// Extra CA certificates (PEM) to trust for relay TLS, for relays with private certificates.
    #[arg(long, env = "IMESH_RELAY_CA")]
    pub relay_ca: Option<PathBuf>,
    /// Do not use n0 relays or n0 DNS lookup.
    #[arg(long, env = "IMESH_NO_N0")]
    pub no_n0: bool,
    /// Also run an iroh relay on this TCP address, for example 0.0.0.0:3340. Only members,
    /// authorized CLI keys, and this machine may use it.
    #[arg(long, env = "IMESH_SERVE_RELAY")]
    pub serve_relay: Option<SocketAddr>,
    /// Also require this bearer token on the served relay.
    #[arg(long, env = "IMESH_SERVE_RELAY_TOKEN", hide_env_values = true)]
    pub serve_relay_token: Option<String>,
    /// Certificate chain (PEM) to serve the relay over HTTPS. Needs --serve-relay-key.
    #[arg(long, env = "IMESH_SERVE_RELAY_CERT", requires = "serve_relay_key")]
    pub serve_relay_cert: Option<PathBuf>,
    /// Private key (PEM) for --serve-relay-cert.
    #[arg(long, env = "IMESH_SERVE_RELAY_KEY", requires = "serve_relay_cert")]
    pub serve_relay_key: Option<PathBuf>,
    /// Also publish and resolve addresses through this pkarr server (a self-hosted iroh-dns-server),
    /// over HTTP. For example `https://dns.example.com/pkarr`
    #[arg(long, env = "IMESH_PKARR")]
    pub pkarr: Option<url::Url>,
    /// Also find machines on the same LAN with mDNS.
    #[arg(long, env = "IMESH_MDNS")]
    pub mdns: bool,
    /// Open no UDP port: every connection goes out to a relay over HTTPS, and nothing listens for
    /// iroh here. Slower; use your own relay (--relay) for real traffic.
    #[arg(long, env = "IMESH_RELAY_ONLY")]
    pub relay_only: bool,
    /// Publish the served relay through a tunnel: `devtunnel` (found or created by labels) or
    /// `devtunnel:<tunnel id>`. Needs --serve-relay, now or from the settings a machine has.
    #[arg(long, env = "IMESH_EXPOSE_RELAY")]
    pub expose_relay: Option<Expose>,
    /// Use a private relay through the logged-in devtunnel CLI: devtunnel:<id>:<port>.
    /// The port must be free locally. Do not also set --relay or --proxy.
    #[arg(long, env = "IMESH_DEVTUNNEL", conflicts_with_all = ["relays", "proxy", "serve_relay"])]
    pub devtunnel: Option<net::DevTunnel>,
    /// HTTP proxy for relay traffic. The daemon also takes `HTTPS_PROXY` or `HTTP_PROXY`.
    #[arg(long)]
    pub proxy: Option<url::Url>,
}

impl NetArgs {
    pub fn config(&self) -> net::Config {
        net::Config {
            relays: self.devtunnel.as_ref().map(|t| vec![t.relay_url()]).unwrap_or_else(|| self.relays.clone()),
            relay_token: self.relay_token.clone(),
            relay_ca: self.relay_ca.clone(),
            no_n0: self.no_n0,
            pkarr: self.pkarr.clone(),
            mdns: self.mdns,
            serve_relay: self.serve_relay,
            serve_relay_token: self.serve_relay_token.clone(),
            serve_relay_tls: self.serve_relay_cert.clone().zip(self.serve_relay_key.clone()),
            expose_relay: self.expose_relay.clone(),
            devtunnel: self.devtunnel.clone(),
            proxy: self.proxy.clone(),
            relay_only: self.relay_only,
        }
    }

    /// Removes old settings that conflict with an explicit change of relay transport.
    pub fn clear_replaced_env(&self, env: &mut BTreeMap<String, String>) {
        if self.devtunnel.is_some() {
            for key in [
                "IMESH_RELAYS",
                "HTTPS_PROXY",
                "HTTP_PROXY",
                "IMESH_SERVE_RELAY",
                "IMESH_SERVE_RELAY_TOKEN",
                "IMESH_SERVE_RELAY_CERT",
                "IMESH_SERVE_RELAY_KEY",
                "IMESH_EXPOSE_RELAY",
            ] {
                env.remove(key);
            }
        } else if !self.relays.is_empty() || self.expose_relay.is_some() {
            env.remove("IMESH_DEVTUNNEL");
        }
    }

    /// The settings as environment variables for the daemon.
    pub fn to_env(&self) -> BTreeMap<String, String> {
        let mut e = BTreeMap::new();
        let mut set = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                e.insert(k.to_string(), v);
            }
        };
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let join = self.relays.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",");
        set("IMESH_RELAYS", (!self.relays.is_empty()).then_some(join));
        set("IMESH_RELAY_TOKEN", self.relay_token.clone());
        set("IMESH_RELAY_CA", path(&self.relay_ca));
        set("IMESH_NO_N0", self.no_n0.then(|| "true".into()));
        set("IMESH_PKARR", self.pkarr.as_ref().map(|u| u.to_string()));
        set("IMESH_MDNS", self.mdns.then(|| "true".into()));
        set("IMESH_RELAY_ONLY", self.relay_only.then(|| "true".into()));
        set("IMESH_SERVE_RELAY", self.serve_relay.map(|a| a.to_string()));
        set("IMESH_SERVE_RELAY_TOKEN", self.serve_relay_token.clone());
        set("IMESH_SERVE_RELAY_CERT", path(&self.serve_relay_cert));
        set("IMESH_SERVE_RELAY_KEY", path(&self.serve_relay_key));
        set("IMESH_DEVTUNNEL", self.devtunnel.as_ref().map(|t| t.to_string()));
        set("IMESH_EXPOSE_RELAY", self.expose_relay.as_ref().map(|x| x.to_string()));
        set("HTTPS_PROXY", self.proxy.as_ref().map(|u| u.to_string()));
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_to_private_forwarding_clears_host_settings() {
        let mut env = BTreeMap::from([
            ("IMESH_RELAYS".into(), "http://old:3340".into()),
            ("IMESH_SERVE_RELAY".into(), "127.0.0.1:3340".into()),
            ("IMESH_EXPOSE_RELAY".into(), "devtunnel:old".into()),
            ("IMESH_SERVE_RELAY_TOKEN".into(), "old-token".into()),
            ("HTTPS_PROXY".into(), "http://proxy:8080".into()),
            ("IMESH_NAME".into(), "peer".into()),
        ]);
        let net = NetArgs { devtunnel: Some("devtunnel:private:3340".parse().unwrap()), ..Default::default() };
        net.clear_replaced_env(&mut env);
        env.extend(net.to_env());
        assert_eq!(
            env,
            BTreeMap::from([
                ("IMESH_NAME".into(), "peer".into()),
                ("IMESH_DEVTUNNEL".into(), "devtunnel:private:3340".into()),
            ])
        );
        let net = NetArgs { relays: vec!["http://relay:3340".parse().unwrap()], ..Default::default() };
        net.clear_replaced_env(&mut env);
        assert!(!env.contains_key("IMESH_DEVTUNNEL"));
    }
}
