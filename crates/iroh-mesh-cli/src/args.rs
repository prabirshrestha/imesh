//! The daemon's flags, and the `IMESH_*` variables that set them. They map to
//! [`iroh_mesh::daemon::Config`], and an install writes them to the daemon's env file.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use anyhow::Result;
use iroh::RelayUrl;
use iroh_mesh::{
    daemon::{self, Access, LocalApi},
    net::{self, Expose},
};

use crate::paths;

/// `imesh service run`.
#[derive(Debug, Clone, clap::Args)]
pub struct DaemonArgs {
    /// Where the machine key and state live.
    #[arg(long, env = "IMESH_HOME", default_value_os_t = paths::data_dir())]
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
                // These relays are on loopback, never behind a proxy. devtunnel itself still reads
                // HTTPS_PROXY from the environment.
                proxy: if self.settings.net.devtunnel.is_some()
                    || self.settings.net.expose_relay.is_some()
                    || self.settings.net.has(&Relay::DevTunnel)
                {
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
    /// Serve SSH (`imesh ssh`, `imesh scp`) to the other machines of the cluster, as this user.
    /// Not for a system daemon. `--ssh=false` turns it off. `up` keeps the choice.
    #[arg(long, env = "IMESH_SSH", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub ssh: Option<bool>,
    /// Record every SSH terminal session on this machine, as asciinema files in the data
    /// directory (`ssh-recordings`). Needs --ssh.
    #[arg(long, env = "IMESH_SSH_RECORD", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub ssh_record: Option<bool>,
    /// Labels for the machine, as key=value. Repeat for more.
    #[arg(long = "label", env = "IMESH_LABELS", value_delimiter = ',', value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub net: NetArgs,
}

impl Settings {
    /// Removes saved settings that these settings turn off.
    pub fn clear_replaced_env(&self, env: &mut BTreeMap<String, String>) {
        self.net.clear_replaced_env(env);
        if self.ssh == Some(false) {
            for k in ["IMESH_SSH", "IMESH_SSH_RECORD"] {
                env.remove(k);
            }
        }
        // `--flag=false` turns off a switch that an earlier `up` saved.
        for (v, k) in [
            (self.ssh_record, "IMESH_SSH_RECORD"),
            (self.net.mdns, "IMESH_MDNS"),
            (self.net.relay_only, "IMESH_RELAY_ONLY"),
        ] {
            if v == Some(false) {
                env.remove(k);
            }
        }
    }

    pub fn ssh(&self) -> bool {
        self.ssh == Some(true)
    }

    /// The settings as environment variables for the daemon.
    pub fn to_env(&self) -> BTreeMap<String, String> {
        let mut env = self.net.to_env();
        if self.ssh == Some(true) {
            env.insert("IMESH_SSH".into(), "true".into());
        }
        if self.ssh_record == Some(true) {
            env.insert("IMESH_SSH_RECORD".into(), "true".into());
        }
        if !self.labels.is_empty() {
            let labels: Vec<_> = self.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            env.insert("IMESH_LABELS".into(), labels.join(","));
        }
        env
    }
}

/// Parses `90s`, `30m`, `12h`, or `7d`.
pub(crate) fn parse_period(s: &str) -> Result<std::time::Duration, String> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().map_err(|_| format!("{s:?} is not a period like 30m or 12h"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" | "" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("{s:?} is not a period like 30m or 12h")),
    };
    if secs == 0 {
        return Err("the period must be longer than 0".into());
    }
    Ok(std::time::Duration::from_secs(secs))
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

/// One `--relay` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relay {
    /// The public n0 relays and n0 address lookup.
    N0,
    /// The devtunnel mesh.
    DevTunnel,
    Url(RelayUrl),
    /// No relay, on purpose: a join with a devtunnel mesh code keeps it.
    None,
}

impl std::str::FromStr for Relay {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "n0" => Ok(Relay::N0),
            "devtunnel" => Ok(Relay::DevTunnel),
            "none" => Ok(Relay::None),
            _ => s
                .parse::<RelayUrl>()
                .map(Relay::Url)
                .map_err(|e| format!("{s:?} is not n0, devtunnel, none, or a relay URL: {e}")),
        }
    }
}

impl std::fmt::Display for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Relay::N0 => f.write_str("n0"),
            Relay::DevTunnel => f.write_str("devtunnel"),
            Relay::Url(u) => write!(f, "{u}"),
            Relay::None => f.write_str("none"),
        }
    }
}

/// Network settings: see [`net::Config`].
#[derive(Debug, Clone, Default, clap::Args)]
#[group(skip)]
pub struct NetArgs {
    /// Relays to use. Repeat or separate with commas. `devtunnel`: the devtunnel mesh, where this
    /// machine serves a relay through a private devtunnel and connects to every member's, with its
    /// own devtunnel login. `n0`: the public n0 relays and n0 address lookup. `none`: no relay,
    /// also when `up --join` would add the devtunnel mesh. Else a relay URL. None by default.
    #[arg(long = "relay", env = "IMESH_RELAYS", value_delimiter = ',')]
    pub relays: Vec<Relay>,
    /// Where the iroh endpoint listens for UDP, for example 0.0.0.0:7842. Default: every
    /// interface, a random port.
    #[arg(long, env = "IMESH_BIND")]
    pub bind: Option<SocketAddr>,
    /// The first loopback port of the cluster's gossip range. It uses 20000 ports from here. Every
    /// member of a cluster must use the same value. Default: 20000.
    #[arg(long, env = "IMESH_GOSSIP_PORT_BASE")]
    pub gossip_port_base: Option<u16>,
    /// The loopback port of this machine's relay with `--relay devtunnel`. Default: a port from
    /// the machine key, 49152 to 65534.
    #[arg(long, env = "IMESH_MESH_PORT")]
    pub mesh_port: Option<u16>,
    /// Bearer token to present to relays that require one.
    #[arg(long, env = "IMESH_RELAY_TOKEN", hide_env_values = true)]
    pub relay_token: Option<String>,
    /// Extra CA certificates (PEM) to trust for relay TLS, for relays with private certificates.
    #[arg(long, env = "IMESH_RELAY_CA")]
    pub relay_ca: Option<PathBuf>,
    /// Also run an iroh relay on this TCP address, for example 0.0.0.0:3340. Only members and
    /// this machine may use it.
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
    #[arg(long, env = "IMESH_MDNS", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub mdns: Option<bool>,
    /// Open no UDP port: every connection goes out to a relay over HTTPS, and nothing listens for
    /// iroh here. Slower; use your own relay (--relay) for real traffic.
    #[arg(long, env = "IMESH_RELAY_ONLY", num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub relay_only: Option<bool>,
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
    /// The devtunnel CLI for the daemon. The install finds it on the PATH of the shell that runs
    /// the install, because a daemon started at boot may have a shorter PATH.
    #[arg(long, env = "IMESH_DEVTUNNEL_CLI", hide = true)]
    pub devtunnel_cli: Option<PathBuf>,
}

impl NetArgs {
    pub fn has(&self, r: &Relay) -> bool {
        self.relays.contains(r)
    }

    /// The relay URLs, without `n0` and `devtunnel`.
    pub fn urls(&self) -> Vec<RelayUrl> {
        self.relays
            .iter()
            .filter_map(|r| match r {
                Relay::Url(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn config(&self) -> net::Config {
        net::Config {
            relays: self.devtunnel.as_ref().map(|t| vec![t.relay_url()]).unwrap_or_else(|| self.urls()),
            relay_token: self.relay_token.clone(),
            relay_ca: self.relay_ca.clone(),
            n0: self.has(&Relay::N0),
            bind: self.bind,
            gossip_port_base: self.gossip_port_base,
            mesh_port: self.mesh_port,
            devtunnel_mesh: self.has(&Relay::DevTunnel),
            pkarr: self.pkarr.clone(),
            mdns: self.mdns == Some(true),
            serve_relay: self.serve_relay,
            serve_relay_token: self.serve_relay_token.clone(),
            serve_relay_tls: self.serve_relay_cert.clone().zip(self.serve_relay_key.clone()),
            expose_relay: self.expose_relay.clone(),
            devtunnel: self.devtunnel.clone(),
            proxy: self.proxy.clone(),
            relay_only: self.relay_only == Some(true),
            devtunnel_cli: self.devtunnel_cli.clone(),
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
        set("IMESH_BIND", self.bind.map(|a| a.to_string()));
        set("IMESH_GOSSIP_PORT_BASE", self.gossip_port_base.map(|p| p.to_string()));
        set("IMESH_MESH_PORT", self.mesh_port.map(|p| p.to_string()));
        set("IMESH_RELAY_CA", path(&self.relay_ca));
        set("IMESH_PKARR", self.pkarr.as_ref().map(|u| u.to_string()));
        set("IMESH_MDNS", (self.mdns == Some(true)).then(|| "true".into()));
        set("IMESH_RELAY_ONLY", (self.relay_only == Some(true)).then(|| "true".into()));
        set("IMESH_SERVE_RELAY", self.serve_relay.map(|a| a.to_string()));
        set("IMESH_SERVE_RELAY_TOKEN", self.serve_relay_token.clone());
        set("IMESH_SERVE_RELAY_CERT", path(&self.serve_relay_cert));
        set("IMESH_SERVE_RELAY_KEY", path(&self.serve_relay_key));
        set("IMESH_DEVTUNNEL", self.devtunnel.as_ref().map(|t| t.to_string()));
        set("IMESH_EXPOSE_RELAY", self.expose_relay.as_ref().map(|x| x.to_string()));
        set("HTTPS_PROXY", self.proxy.as_ref().map(|u| u.to_string()));
        set("IMESH_DEVTUNNEL_CLI", path(&self.devtunnel_cli));
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
        assert_eq!(net.urls(), vec!["http://relay:3340".parse::<RelayUrl>().unwrap()]);
        let both: Vec<Relay> = ["n0", "devtunnel"].iter().map(|s| s.parse().unwrap()).collect();
        assert_eq!(both, vec![Relay::N0, Relay::DevTunnel]);
        assert!("nope".parse::<Relay>().is_err());
        let none = NetArgs { relays: vec!["none".parse().unwrap()], ..Default::default() };
        let c = none.config();
        assert!(c.relays.is_empty() && !c.n0 && !c.devtunnel_mesh, "`none` means no relay");
        assert_eq!(none.to_env().get("IMESH_RELAYS").map(String::as_str), Some("none"), "and is saved");
        net.clear_replaced_env(&mut env);
        assert!(!env.contains_key("IMESH_DEVTUNNEL"));
    }
}

#[cfg(test)]
mod ssh_setting_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        s: Settings,
    }

    fn merge(saved: &[(&str, &str)], args: &[&str]) -> BTreeMap<String, String> {
        let s = T::try_parse_from(std::iter::once("t").chain(args.iter().copied())).unwrap().s;
        let mut env: BTreeMap<String, String> = saved.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        s.clear_replaced_env(&mut env);
        env.extend(s.to_env());
        env
    }

    #[test]
    fn up_keeps_ssh_until_turned_off() {
        let saved = [("IMESH_SSH", "true"), ("IMESH_SSH_RECORD", "true"), ("IMESH_LABELS", "a=b")];
        assert_eq!(merge(&saved, &[]).get("IMESH_SSH").map(String::as_str), Some("true"));
        let off = merge(&saved, &["--ssh=false"]);
        assert!(!off.contains_key("IMESH_SSH") && !off.contains_key("IMESH_SSH_RECORD"));
        assert_eq!(off.get("IMESH_LABELS").map(String::as_str), Some("a=b"));
        assert_eq!(merge(&[], &["--ssh"]).get("IMESH_SSH").map(String::as_str), Some("true"));
        let saved = [("IMESH_SSH_RECORD", "true"), ("IMESH_RELAY_ONLY", "true")];
        let off = merge(&saved, &["--ssh-record=false", "--relay-only=false"]);
        assert!(off.is_empty(), "{off:?}");
    }
}

/// Parses a member name with the daemon's rules, so a bad name fails before any install.
pub(crate) fn member_name(s: &str) -> Result<String, String> {
    iroh_mesh::daemon::valid_member_name(s).map(|()| s.to_string()).map_err(|e| e.to_string())
}
