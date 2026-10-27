//! Cluster operations on a machine: start a cluster, admit and join machines, remove them, and
//! manage CLI keys and relays. The API methods in `methods.rs` call these; an app that embeds a
//! machine can call them directly.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use rusqlite::params;

use super::Machine;
use crate::{
    api::{ClientRow, JoinTicket, Joined, MachineRow},
    net,
    state::{RelaySettings, schema},
};

/// How long a join waits for a member to send this machine its own member row.
const JOIN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `machine.reset` waits at most for its reply to reach the caller.
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
    pub async fn admit(&self, id: EndpointId, name: &str) -> Result<JoinTicket> {
        let snap = self.store.snapshot();
        let cluster = snap.cluster.clone().context("machine is not in a cluster")?;
        ensure!(!snap.is_removed(&id), "machine {} was removed; reset it to get a new identity", id.fmt_short());
        ensure!(
            !snap.members.iter().any(|(other, m)| !m.removed && m.name == name && *other != id),
            "name {name:?} is taken"
        );
        let key = id.to_string();
        self.store.write(|c| upsert(c, "imesh_members", "id", "name", &key, name)).await?;
        self.online().await;
        let others = self.members().into_iter().filter(|m| *m != self.id() && *m != id);
        let peers = std::iter::once(self.endpoint.addr()).chain(others.map(|m| self.peer_addr(m))).collect();
        Ok(JoinTicket { cluster: cluster.id, peers, relays: snap.relay_settings() })
    }

    /// Joins the cluster of `ticket`: links to its members, and waits until Corrosion has brought
    /// this machine its own member row. A machine that holds rows of the same cluster from a
    /// failed join may try again; rows of another cluster need a reset first.
    pub async fn join(&self, ticket: &JoinTicket) -> Result<Joined> {
        let me = self.id();
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
        let snap = self.store.snapshot();
        self.apply_cluster_relays(&snap).await;
        self.reconcile_links();
        ensure!(joined, "no member sent this machine its member row within {JOIN_TIMEOUT:?}");
        let c = snap.cluster.context("no cluster row after sync")?;
        ensure!(c.id == ticket.cluster, "members are in cluster {}, expected {}", c.id, ticket.cluster);
        self.publish_self().await;
        Ok(Joined { cluster: c.id, members: self.members().len() })
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

    /// Lets the iroh key `key` call the API of every machine: full control of the cluster.
    pub async fn authorize_client(&self, key: EndpointId, name: &str) -> Result<()> {
        ensure!(self.cluster().is_some(), "machine is not in a cluster");
        let key = key.to_string();
        self.store.write(|c| upsert(c, "imesh_clients", "key", "name", &key, name)).await?;
        Ok(())
    }

    /// Revokes a key from [`Machine::authorize_client`]. Its open connections close.
    pub async fn revoke_client(&self, key: EndpointId) -> Result<()> {
        ensure!(self.store.read(|s| s.clients.contains_key(&key)), "not an authorized CLI key");
        let key = key.to_string();
        self.store.write(|c| c.execute("DELETE FROM imesh_clients WHERE key = ?1", params![key])).await?;
        Ok(())
    }

    pub fn clients(&self) -> Vec<ClientRow> {
        self.store.read(|s| s.clients.iter().map(|(key, name)| ClientRow { key: *key, name: name.clone() }).collect())
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

    /// Writes a JSON value to the built-in `imesh_kv` table. `None` deletes the key.
    pub async fn kv_put(&self, key: &str, value: Option<&serde_json::Value>) -> Result<()> {
        match value {
            Some(v) => {
                let v = serde_json::to_string(v)?;
                self.store.write(|c| upsert(c, "imesh_kv", "key", "value", key, &v)).await?
            }
            None => self.store.write(|c| c.execute("DELETE FROM imesh_kv WHERE key = ?1", params![key])).await?,
        };
        Ok(())
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
