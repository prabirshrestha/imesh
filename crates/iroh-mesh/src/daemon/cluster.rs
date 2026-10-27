//! Cluster operations on a machine: start a cluster, admit and join machines, remove them, and
//! manage relays. The API methods in `methods.rs` call these; an app that embeds a
//! machine can call them directly.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use rusqlite::params;

use super::Machine;
use crate::{
    api::{JoinTicket, Joined, MachineRow},
    net,
    state::{RelaySettings, schema},
};

/// How long a join waits for a member to send this machine its own member row.
const JOIN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `machine.reset` waits at most for its reply to reach the caller.
/// A member name: letters, digits, '-', '_', and '.', and never `self`, which the CLI uses for
/// this computer's machine.
pub fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "a name has letters, digits, '-', '_', and '.' only"
    );
    ensure!(!name.eq_ignore_ascii_case("self"), "`self` is reserved for this computer's machine; pick another name");
    Ok(())
}

/// How long an expired key stays before cleanup deletes it. Reads stop showing a key when it
/// expires; the delete waits much longer, so a machine whose clock is ahead of the writer's by
/// less than this never deletes a key that the writer still counts as live.
pub(crate) const KV_EXPIRE_GRACE: Duration = Duration::from_secs(3600);

/// Seconds since the Unix epoch, by this machine's clock.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub(super) const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Inserts a row or replaces its `value_col`. Table and column names are constants of this module.
fn upsert(
    c: &rusqlite::Connection,
    table: &str,
    key_col: &str,
    value_col: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<usize> {
    c.execute(
        &format!(
            "INSERT INTO {table} ({key_col}, {value_col}) VALUES (?1, ?2) \
             ON CONFLICT ({key_col}) DO UPDATE SET {value_col} = excluded.{value_col}"
        ),
        params![key, value],
    )
}

impl Machine {
    /// The ID of the cluster this machine is in.
    pub fn cluster(&self) -> Option<String> {
        self.store.read(|s| s.cluster.as_ref().map(|c| c.id.clone()))
    }

    /// Starts a new cluster with this machine as its only member. `name` is this machine's name
    /// in the cluster; the default is [`Machine::name`]. Returns the cluster ID.
    pub async fn init_cluster(&self, name: Option<String>) -> Result<String> {
        ensure!(self.cluster().is_none(), "machine is already in a cluster");
        let id = hex::encode(rand::random::<[u8; 16]>());
        let (me, name) = (self.id().to_string(), name.unwrap_or_else(|| self.name.clone()));
        valid_name(&name)?;
        self.store
            .write(|c| {
                c.execute("INSERT INTO imesh_cluster (id, founder) VALUES (?1, ?2)", params![id, me])?;
                c.execute("INSERT INTO imesh_members (id, name) VALUES (?1, ?2)", params![me, name])
            })
            .await?;
        self.online().await;
        self.publish_self().await;
        Ok(id)
    }

    /// Admits the machine with key `id` under `name`, and returns the ticket it joins with. The
    /// members accept its key from now on.
    pub async fn admit(
        &self,
        id: EndpointId,
        name: &str,
        tunnel: Option<crate::state::TunnelEndpoint>,
    ) -> Result<JoinTicket> {
        valid_name(name)?;
        let snap = self.store.snapshot();
        let cluster = snap.cluster.clone().context("machine is not in a cluster")?;
        ensure!(!snap.is_removed(&id), "machine {} was removed; reset it to get a new identity", id.fmt_short());
        ensure!(
            !snap.members.iter().any(|(other, m)| !m.removed && m.name == name && *other != id),
            "name {name:?} is taken"
        );
        // Every member reaches each other one at fixed loopback ports, so ports must not collide.
        let base = self.store.gossip_base;
        let tunnels = self.member_tunnels(&snap);
        for other in self.members().into_iter().filter(|m| *m != id) {
            ensure!(
                crate::state::slot(base, &other) != crate::state::slot(base, &id),
                "machine {} has the gossip ports of member {}; reset it for a new key with `imesh call machine.reset`",
                id.fmt_short(),
                other.fmt_short()
            );
        }
        if let Some(t) = &tunnel {
            let mine = self.tunnel_endpoint();
            for (other, e) in tunnels.iter().map(|(i, e)| (*i, e)).chain(mine.as_ref().map(|e| (self.id(), e))) {
                ensure!(
                    other == id || e.port() != t.port(),
                    "machine {} uses mesh port {}, as member {} does; start it with another --mesh-port",
                    id.fmt_short(),
                    t.port(),
                    other.fmt_short()
                );
            }
            tracing::info!(machine = %id.fmt_short(), ?t, "admitting a mesh machine; forwarding its relay");
            self.forward_tunnel(id, t.clone());
        } else {
            tracing::info!(machine = %id.fmt_short(), "admitting a machine with no mesh tunnel");
        }
        let key = id.to_string();
        self.store.write(|c| upsert(c, "imesh_members", "id", "name", &key, name)).await?;
        self.online().await;
        let others = self.members().into_iter().filter(|m| *m != self.id() && *m != id);
        let peers = std::iter::once(self.endpoint.addr()).chain(others.map(|m| self.peer_addr(m))).collect();
        let mut tunnels: Vec<_> = tunnels.into_iter().filter(|(m, _)| *m != id).collect();
        tunnels.extend(self.tunnel_endpoint().map(|e| (self.id(), e)));
        Ok(JoinTicket { cluster: cluster.id, peers, relays: snap.relay_settings(), gossip_port_base: base, tunnels })
    }

    /// The mesh tunnel of each other member, from its row.
    fn member_tunnels(&self, snap: &crate::state::Snapshot) -> Vec<(EndpointId, crate::state::TunnelEndpoint)> {
        let me = self.id();
        self.members().into_iter().filter(|m| *m != me).filter_map(|m| Some((m, snap.machine(&m)?.tunnel?))).collect()
    }

    /// Joins the cluster of `ticket`: links to its members, and waits until Corrosion has brought
    /// this machine its own member row. A machine that holds rows of the same cluster from a
    /// failed join may try again; rows of another cluster need a reset first.
    pub async fn join(&self, ticket: &JoinTicket) -> Result<Joined> {
        let me = self.id();
        ensure!(
            ticket.gossip_port_base == self.store.gossip_base,
            "the cluster uses gossip ports from {}, and this machine from {}; start it with --gossip-port-base {}",
            ticket.gossip_port_base,
            self.store.gossip_base,
            ticket.gossip_port_base
        );
        if let Some(c) = self.cluster() {
            ensure!(c == ticket.cluster, "machine holds cluster {c}; reset it to join another");
            ensure!(!self.members().contains(&me), "machine is already in cluster {c}");
        }
        // Use the cluster's relays before dialing, so a machine that can reach only those relays
        // can join.
        *self.joining.lock().unwrap() = net::relays(&ticket.relays.relays, &ticket.relays.token);
        self.apply_cluster_relays(&self.store.snapshot()).await;
        self.online().await;
        self.admission.set_bootstrap(ticket.peers.iter().map(|a| a.id).collect());
        for addr in &ticket.peers {
            self.lookup.set_endpoint_info(addr.clone());
        }
        // A mesh member has no rows yet: forward the members' tunnels from the ticket first.
        for (id, e) in &ticket.tunnels {
            self.forward_tunnel(*id, e.clone());
        }
        if self.config.net.devtunnel_mesh && !ticket.tunnels.is_empty() {
            self.wait_mesh_ready(&ticket.tunnels).await;
        }
        self.reconcile_links();
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let mut changes = self.store.subscribe();
        let joined = loop {
            if self.store.read(|s| s.cluster.is_some() && s.is_member(&me)) {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            for addr in &ticket.peers {
                self.store.announce(&addr.id).await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), changes.changed()).await;
        };
        self.admission.set_bootstrap(Default::default());
        self.joining.lock().unwrap().clear();
        self.mesh.unpin();
        let snap = self.store.snapshot();
        self.apply_cluster_relays(&snap).await;
        self.reconcile_links();
        ensure!(joined, "no member sent this machine its member row within {JOIN_TIMEOUT:?}");
        let c = snap.cluster.context("no cluster row after sync")?;
        ensure!(c.id == ticket.cluster, "members are in cluster {}, expected {}", c.id, ticket.cluster);
        self.publish_self().await;
        Ok(Joined { cluster: c.id, members: self.members().len() })
    }

    /// Renames member `id` in the cluster. Every member sees the new name. Names are unique.
    pub async fn rename_member(&self, id: EndpointId, name: &str) -> Result<()> {
        valid_name(name)?;
        let snap = self.store.snapshot();
        ensure!(snap.members.get(&id).is_some_and(|m| !m.removed), "not a member");
        ensure!(
            !snap.members.iter().any(|(other, m)| !m.removed && m.name == name && *other != id),
            "name {name:?} is taken"
        );
        let hex = name.len() >= 4 && name.chars().all(|c| c.is_ascii_hexdigit());
        ensure!(
            !hex || !snap
                .members
                .keys()
                .any(|other| *other != id && other.to_string().starts_with(&name.to_ascii_lowercase())),
            "name {name:?} is the start of another member's ID; pick another name"
        );
        let (key, name) = (id.to_string(), name.to_string());
        self.store
            .write(move |c| c.execute("UPDATE imesh_members SET name = ?2 WHERE id = ?1", params![key, name]))
            .await?;
        Ok(())
    }

    /// Removes a member for good. It must reset and join with a new key to come back. Its open
    /// connections close.
    pub async fn remove_member(&self, id: EndpointId) -> Result<()> {
        ensure!(id != self.id(), "a machine cannot remove itself; connect through another machine");
        ensure!(self.members().contains(&id), "not a member");
        let key = id.to_string();
        self.store
            .write(|c| {
                c.execute("UPDATE imesh_members SET removed = 1 WHERE id = ?1", params![key])?;
                c.execute("DELETE FROM imesh_machines WHERE id = ?1", params![key])
            })
            .await?;
        Ok(())
    }

    /// Leaves the cluster for good, before this machine's data is deleted: a member that is up
    /// removes this machine, as `imesh rm` would. The removal is written on a member that
    /// stays, so it reaches the others even after this machine is gone. Returns that member and
    /// the devtunnel this machine hosts, for the caller to delete after the daemon stops; deleted
    /// now, the running host would create a new one. Fails if no other member answers.
    pub async fn leave(&self) -> Result<(EndpointId, Option<String>)> {
        ensure!(self.cluster().is_some(), "machine is not in a cluster");
        let me = self.id();
        let others: Vec<EndpointId> = self.members().into_iter().filter(|id| *id != me).collect();
        ensure!(!others.is_empty(), "this machine is the only member; there is no cluster to tell");
        let mut errors = Vec::new();
        for id in others {
            match self.call_member(id, "cluster.leave", serde_json::Value::Null).await {
                Ok(_) => return Ok((id, self.exposed.borrow().as_ref().map(|h| h.tunnel.clone()))),
                Err(e) => errors.push(format!("{}: {e:#}", id.fmt_short())),
            }
        }
        anyhow::bail!("no other member answered: {}", errors.join("; "))
    }

    /// Whether this machine is the only member of its cluster, and the cluster holds no `kv`
    /// rows and no app rows. Such a machine may leave its cluster for another with nothing lost.
    pub async fn alone_and_empty(&self) -> Result<bool> {
        if self.cluster().is_none() || self.members().iter().any(|id| *id != self.id()) {
            return Ok(false);
        }
        let tables: Vec<String> = self.store.tables().0.keys().filter(|t| !t.starts_with("imesh_")).cloned().collect();
        self.store
            .query(move |c| {
                let mut empty =
                    c.query_row("SELECT NOT EXISTS (SELECT 1 FROM imesh_kv)", [], |r| r.get::<_, bool>(0))?;
                for t in &tables {
                    empty &= c.query_row(&format!("SELECT NOT EXISTS (SELECT 1 FROM \"{t}\")"), [], |r| {
                        r.get::<_, bool>(0)
                    })?;
                }
                Ok(empty)
            })
            .await
    }

    /// Accepts `member`'s request to leave. The reply goes first: the removal closes the
    /// member's connections, also the one that carries the reply.
    pub fn accept_leave(&self, member: EndpointId) -> Result<()> {
        ensure!(member != self.id(), "a machine cannot remove itself; connect through another machine");
        ensure!(self.members().contains(&member), "not a member");
        let m = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(REPLY_TIMEOUT).await;
            if let Err(e) = m.remove_member(member).await {
                tracing::warn!(member = %member.fmt_short(), "could not remove the member that left: {e:#}");
            }
        });
        Ok(())
    }

    /// Calls an API method on another member over iroh, as this machine.
    async fn call_member(&self, id: EndpointId, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let conn = self.connect(id, crate::api::API_ALPN).await?;
        let (send, recv) = conn.open_bi().await?;
        let req = crate::api::Request { to: None, method: method.into(), params };
        let reply = tokio::time::timeout(
            Duration::from_secs(30),
            crate::api::call(crate::api::framed(tokio::io::join(recv, send)), &req),
        )
        .await
        .context("no reply")??;
        conn.close(0u32.into(), b"done");
        Ok(reply)
    }

    /// Every member, with its info and how this machine sees it now.
    pub fn machines(&self) -> Vec<MachineRow> {
        let snap = self.store.snapshot();
        snap.members
            .iter()
            .filter(|(_, m)| !m.removed)
            .map(|(id, m)| MachineRow {
                id: *id,
                name: m.name.clone(),
                info: snap.machine(id),
                status: self.peer_status(id),
            })
            .collect()
    }

    /// Makes every machine use exactly these relays. No relays: each machine goes back to the
    /// relays it started with.
    pub async fn set_cluster_relays(&self, settings: &RelaySettings) -> Result<()> {
        let value = serde_json::to_string(settings)?;
        self.store.write(|c| upsert(c, "imesh_settings", "key", "value", "relays", &value)).await?;
        Ok(())
    }

    /// Adds or grows app tables on every machine: `CREATE TABLE` and `CREATE INDEX` statements.
    /// They apply here first, so a change that cannot apply (dropping or changing a table or
    /// column) fails here and reaches no one. Members take the new tables over their links
    /// before any row that uses them.
    pub async fn apply_schema(&self, sql: &str) -> Result<()> {
        for name in schema::table_names(&[sql.to_string()])? {
            ensure!(
                !name.to_ascii_lowercase().starts_with("imesh_"),
                "table {name}: tables named imesh_* belong to imesh"
            );
        }
        self.store.apply_schema(vec![sql.to_string()]).await
    }

    /// Writes a JSON value to the built-in `imesh_kv` table. `None` deletes the key. With `ttl`,
    /// the key expires that long from now, by this machine's clock: reads do not show it after
    /// that, and [`Machine::kv_expire`] deletes it.
    pub async fn kv_put(&self, key: &str, value: Option<&serde_json::Value>, ttl: Option<Duration>) -> Result<()> {
        match value {
            Some(v) => {
                let v = serde_json::to_string(v)?;
                let expires = ttl.map(|t| now_secs() + t.as_secs() as i64);
                self.store
                    .write(|c| {
                        c.execute(
                            "INSERT INTO imesh_kv (key, value, expires_at) VALUES (?1, ?2, ?3) \
                             ON CONFLICT (key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
                            params![key, v, expires],
                        )
                    })
                    .await?
            }
            None => self.store.write(|c| c.execute("DELETE FROM imesh_kv WHERE key = ?1", params![key])).await?,
        };
        Ok(())
    }

    /// Deletes the keys that expired more than [`KV_EXPIRE_GRACE`] ago. Every machine runs it; a key
    /// that another machine set again since it expired has a new `expires_at`, so the delete
    /// leaves it alone.
    pub(crate) async fn kv_expire(&self) -> Result<usize> {
        let before = now_secs() - KV_EXPIRE_GRACE.as_secs() as i64;
        self.store
            .write(|c| {
                c.execute("DELETE FROM imesh_kv WHERE expires_at IS NOT NULL AND expires_at < ?1", params![before])
            })
            .await
    }

    /// Asks the owner of the machine to reset it: a new key, out of any cluster, with its data
    /// deleted. [`super::Running::reset_requested`] resolves; [`super::serve`] does the rest.
    pub fn request_reset(&self) {
        self.reset.send_replace(true);
    }

    /// Resets after the reply to the current API call has left: the API handler calls
    /// [`Machine::finish_reset`] then. A fallback timer resets anyway.
    pub(super) fn reset_after_reply(&self) {
        self.reset_after_reply.store(true, std::sync::atomic::Ordering::Relaxed);
        let m = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(REPLY_TIMEOUT).await;
            m.request_reset();
        });
    }

    /// Called by the API handlers after each reply: starts a reset that waits for it.
    pub(super) fn finish_reset(&self) {
        if self.reset_after_reply.load(std::sync::atomic::Ordering::Relaxed) {
            self.request_reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{Builder, Config};

    #[test]
    fn names_are_checked() {
        for ok in ["mac", "dev-box_1", "a.b"] {
            valid_name(ok).unwrap();
        }
        for bad in ["", "self", "SELF", "a b", "a/b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kv_ttl_and_a_machine_alone_with_nothing_in_its_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new(dir.path());
        config.net.gossip_port_base = Some(40000);
        let running = Builder::new(config).spawn().await.unwrap();
        let m = running.machine.clone();
        assert!(!m.alone_and_empty().await.unwrap(), "in no cluster");
        m.init_cluster(Some("one".into())).await.unwrap();
        assert!(m.alone_and_empty().await.unwrap(), "alone, with no rows");

        let get = |m: Machine, key: &'static str| async move {
            m.state()
                .query(move |c| {
                    c.query_row(
                        "SELECT value FROM imesh_kv WHERE key = ?1 AND (expires_at IS NULL OR expires_at >= ?2)",
                        params![key, now_secs()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                })
                .await
                .unwrap()
        };
        use rusqlite::OptionalExtension;
        m.kv_put("keep", Some(&"v".into()), None).await.unwrap();
        assert!(!m.alone_and_empty().await.unwrap(), "a kv row counts as data");
        m.kv_put("short", Some(&"v".into()), Some(Duration::from_secs(3600))).await.unwrap();
        assert_eq!(get(m.clone(), "short").await.as_deref(), Some("\"v\""));
        m.state()
            .write(|c| c.execute("UPDATE imesh_kv SET expires_at = ?1 WHERE key = 'short'", params![now_secs() - 5]))
            .await
            .unwrap();
        assert_eq!(get(m.clone(), "short").await, None, "an expired key is not read");
        assert_eq!(m.kv_expire().await.unwrap(), 0, "kept during the grace period");
        m.state()
            .write(|c| {
                c.execute("UPDATE imesh_kv SET expires_at = ?1 WHERE key = 'short'", params![now_secs() - 2 * 3600])
            })
            .await
            .unwrap();
        assert_eq!(m.kv_expire().await.unwrap(), 1, "deleted after the grace period");
        assert_eq!(get(m.clone(), "keep").await.as_deref(), Some("\"v\""), "a key with no TTL stays");
        running.shutdown().await.unwrap();
    }
}
