//! Cluster operations on a machine: start a cluster, admit and join machines, remove them, and
//! manage roles and relays. The API methods in `methods.rs` call these; an app that embeds a
//! machine can call them directly.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;

use super::{Caller, Machine};
use crate::{
    api::{Initialized, JoinTicket, Joined, MachineRow},
    net,
    roster::{Change, Relays, Role, Roster, recovery_code, root_from_code},
    state::RelaySettings,
};

/// Where to run a decision, for error text, on a machine that the members refuse as removed.
const REMOVED: &str = "another machine: the members refuse this one as removed";

/// How long `machine.reset` waits at most for its reply to reach the caller.
pub(super) const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A member name: letters, digits, '-', and '_', and never `self`, which the CLI uses for this
/// computer's machine.
pub fn valid_name(name: &str) -> Result<()> {
    ensure!(
        crate::roster::valid_name(name),
        "a name has 1 to {} letters, digits, '-', and '_'",
        crate::roster::MAX_NAME
    );
    ensure!(!name.eq_ignore_ascii_case("self"), "`self` is reserved for this computer's machine; pick another name");
    Ok(())
}

/// Fails when `caller` may not have this machine sign `what`. A remote member acts through this
/// machine only where its own role, as `may` judges it, lets it act. Else a plain member could
/// make the owner's machine sign what the member may not sign. Run it in [`crate::state::Store::sign_if`],
/// so the role cannot change before the signature.
fn caller_may(r: &Roster, caller: Caller, may: impl FnOnce(Option<Role>) -> bool, what: &str) -> Result<()> {
    if let Caller::Member(id) = caller {
        let role = r.role(&id);
        ensure!(may(role), "a {} cannot {what} through another machine", role_name(role));
    }
    Ok(())
}

fn role_name(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::Owner) => "owner",
        Some(Role::Admin) => "admin",
        Some(Role::Member) => "plain member",
        None => "machine outside the cluster",
    }
}

fn decides(role: Option<Role>) -> bool {
    matches!(role, Some(Role::Owner | Role::Admin))
}

impl Machine {
    /// The ID of the cluster this machine is in: the root public key, as text.
    pub fn cluster(&self) -> Option<String> {
        self.store.read(|s| s.cluster.as_ref().map(|c| c.id.clone()))
    }

    /// This machine's role in its cluster. None while the members refuse it as removed: the
    /// members do not send their roster to a removed machine, so its own copy may still give it
    /// the role it had, and nothing it signs then counts.
    pub fn role(&self) -> Option<Role> {
        if self.cut_off.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        self.store.read(|s| s.role(&self.id()))
    }

    /// Starts a new cluster with this machine as its owner. `name` is this machine's name in the
    /// cluster; the default is [`Machine::name`]. Returns the cluster ID and the recovery code,
    /// which no machine keeps: show it once.
    pub async fn init_cluster(&self, name: Option<String>) -> Result<Initialized> {
        let name = name.unwrap_or_else(|| self.name.clone());
        valid_name(&name)?;
        let root = self.store.create(self.endpoint.secret_key(), name)?;
        self.online().await;
        self.publish_self().await;
        Ok(Initialized { cluster: root.public().to_string(), recovery_code: recovery_code(&root) })
    }

    /// Admits the machine with key `id` under `name`, for `caller`, and returns the ticket it
    /// joins with. Only the owner and admins may admit.
    pub async fn admit(
        &self,
        caller: Caller,
        id: EndpointId,
        name: &str,
        tunnel: Option<crate::state::TunnelEndpoint>,
    ) -> Result<JoinTicket> {
        valid_name(name)?;
        let snap = self.store.snapshot();
        let cluster = snap.cluster.clone().context("machine is not in a cluster")?;
        let add = "add a machine";
        caller_may(&self.store.roster().context("machine is not in a cluster")?, caller, decides, add)?;
        ensure!(!snap.is_removed(&id), "machine {} was removed; reset it to get a new identity", id.fmt_short());
        ensure!(
            !snap.members.iter().any(|(other, m)| m.name.eq_ignore_ascii_case(name) && *other != id),
            "name {name:?} is taken"
        );
        ensure!(
            matches!(self.role(), Some(Role::Owner | Role::Admin)),
            "only the owner and admins can add machines; run this on {}",
            self.deciders(&snap)
        );
        if let Some(t) = &tunnel {
            tracing::info!(machine = %id.fmt_short(), ?t, "admitting a mesh machine; forwarding its relay");
            self.forward_tunnel(id, t.clone());
        } else {
            tracing::info!(machine = %id.fmt_short(), "admitting a machine with no mesh tunnel");
        }
        if !snap.is_member(&id) {
            let admit = Change::Admit { id, name: name.to_string() };
            self.store.sign_if(self.endpoint.secret_key(), |r| caller_may(r, caller, decides, add), |_| admit)?;
        }
        self.online().await;
        let others = self.members().into_iter().filter(|m| *m != self.id() && *m != id);
        let peers = std::iter::once(self.endpoint.addr()).chain(others.map(|m| self.peer_addr(m))).collect();
        let mut tunnels: Vec<_> = self.member_tunnels(&snap).into_iter().filter(|(m, _)| *m != id).collect();
        tunnels.extend(self.tunnel_endpoint().map(|e| (self.id(), e)));
        let roster = self.store.roster().context("machine is not in a cluster")?;
        Ok(JoinTicket { cluster: cluster.id, peers, relays: snap.relay_settings(), tunnels, roster })
    }

    /// The names of the machines that may add and remove machines, for error text.
    fn deciders(&self, snap: &crate::state::Snapshot) -> String {
        if self.cut_off.load(std::sync::atomic::Ordering::Relaxed) {
            return REMOVED.into();
        }
        let names: Vec<&str> = snap
            .members
            .values()
            .filter(|m| matches!(m.role, Role::Owner | Role::Admin))
            .map(|m| m.name.as_str())
            .collect();
        if names.is_empty() { "the owner".into() } else { names.join(" or ") }
    }

    /// The mesh tunnel of each other member, from the roster.
    fn member_tunnels(&self, snap: &crate::state::Snapshot) -> Vec<(EndpointId, crate::state::TunnelEndpoint)> {
        let me = self.id();
        snap.machines.iter().filter(|(id, _)| **id != me).filter_map(|(id, m)| Some((*id, m.tunnel.clone()?))).collect()
    }

    /// Joins the cluster of `ticket`: takes the ticket's roster, which already admits this
    /// machine, and tells the members where this machine is.
    pub async fn join(&self, ticket: &JoinTicket) -> Result<Joined> {
        let me = self.id();
        let network: EndpointId = ticket.cluster.parse().context("bad cluster id in the ticket")?;
        ensure!(ticket.roster.network == network, "the ticket's roster is for another cluster");
        if let Some(c) = self.cluster() {
            ensure!(c == ticket.cluster, "machine holds cluster {c}; reset it to join another");
            ensure!(!self.members().contains(&me), "machine is already in cluster {c}");
        }
        // Use the cluster's relays before dialing, so a machine that can reach only those relays
        // can join.
        *self.joining.lock().unwrap() = net::relays(&ticket.relays.relays, &ticket.relays.token);
        self.apply_cluster_relays(&self.store.snapshot()).await;
        self.online().await;
        for addr in &ticket.peers {
            self.lookup.set_endpoint_info(self.reachable(addr.clone()));
        }
        for (id, e) in &ticket.tunnels {
            self.forward_tunnel(*id, e.clone());
        }
        if self.config.net.devtunnel_mesh && !ticket.tunnels.is_empty() {
            self.wait_mesh_ready(&ticket.tunnels).await;
        }
        self.store.merge(ticket.roster.clone(), Some(network)).await?;
        self.joining.lock().unwrap().clear();
        self.mesh.unpin();
        let snap = self.store.snapshot();
        self.apply_cluster_relays(&snap).await;
        ensure!(snap.is_member(&me), "the ticket's roster does not admit this machine; ask for a new invite");
        self.publish_self().await;
        Ok(Joined { cluster: ticket.cluster.clone(), members: self.members().len() })
    }

    /// Removes a member for good. It must reset and join with a new key to come back. Its open
    /// connections close. With `cascade`, every admission, promotion, and removal it made is void
    /// too, for a machine that was stolen. Only the owner and admins may remove others; admins
    /// remove plain members only.
    pub async fn remove_member(&self, caller: Caller, id: EndpointId, cascade: bool) -> Result<()> {
        ensure!(id != self.id(), "a machine does not remove itself; run `imesh rm` on another machine, or leave");
        let snap = self.store.snapshot();
        ensure!(snap.is_member(&id), "not a member");
        let mine = self.role();
        let theirs = snap.role(&id);
        ensure!(
            mine == Some(Role::Owner) || (mine == Some(Role::Admin) && theirs == Some(Role::Member)),
            "only the owner can remove an admin, and only the owner and admins can remove members; run this on {}",
            self.deciders(&snap)
        );
        let allowed = |r: &Roster| {
            let theirs = r.role(&id);
            let may = |mine| mine == Some(Role::Owner) || (mine == Some(Role::Admin) && theirs == Some(Role::Member));
            caller_may(r, caller, may, "remove this machine")
        };
        self.store.sign_if(self.endpoint.secret_key(), allowed, |r| r.removal(id, cascade))?;
        self.hello_all().await;
        Ok(())
    }

    /// Leaves the cluster for good, before this machine's data is deleted: signs its own removal
    /// and hands the roster to a member that answers, so the removal reaches the others after
    /// this machine is gone. Returns that member and the devtunnel this machine hosts, for the
    /// caller to delete after the daemon stops. Fails if no other member answers.
    pub async fn leave(&self) -> Result<(EndpointId, Option<String>)> {
        ensure!(self.cluster().is_some(), "machine is not in a cluster");
        let me = self.id();
        let others: Vec<EndpointId> = self.members().into_iter().filter(|id| *id != me).collect();
        ensure!(!others.is_empty(), "this machine is the only member; there is no cluster to tell");
        ensure!(self.role() != Some(Role::Owner), "the owner cannot leave; move the owner with `imesh recover` first");
        // The removal keeps what this machine decided: the machines it admitted stay, and the
        // machines it removed stay out. A machine that is out already, as after a leave that no
        // member answered, only hands its removal on.
        if !self.store.read(|s| s.is_removed(&me)) {
            self.store.remove(self.endpoint.secret_key(), me, false)?;
        }
        let mut errors = Vec::new();
        for id in others {
            let status = self.fresh_hello(id).await;
            // A member that refused this machine as removed has the removal already.
            if status.error.is_none() || status.refused_as_removed {
                return Ok((id, self.exposed.borrow().as_ref().map(|h| h.tunnel.clone())));
            }
            errors.push(format!("{}: {}", id.fmt_short(), status.error.unwrap_or_default()));
        }
        anyhow::bail!(
            "this machine signed its own removal, but no other member answered to take it: {}. The machine is out of \
             the cluster for good; run `imesh logout` again when a member is up, so the removal reaches the others",
            errors.join("; ")
        )
    }

    /// Whether this machine is the only member of its cluster and keeps no app files. Such a
    /// machine may leave its cluster for another with nothing lost: a reset deletes the app files.
    pub fn alone(&self) -> bool {
        let no_app_files = match std::fs::read_dir(self.store.dir().join("app")) {
            Ok(mut d) => d.next().is_none(),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        self.cluster().is_some() && self.members().iter().all(|id| *id == self.id()) && no_app_files
    }

    /// Renames member `id`. A machine renames itself; the owner and admins rename anyone. Names
    /// are unique.
    pub fn rename_member(&self, caller: Caller, id: EndpointId, name: &str) -> Result<()> {
        valid_name(name)?;
        let snap = self.store.snapshot();
        ensure!(snap.is_member(&id), "not a member");
        ensure!(
            !snap.members.iter().any(|(other, m)| m.name.eq_ignore_ascii_case(name) && *other != id),
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
        ensure!(
            id == self.id() || matches!(self.role(), Some(Role::Owner | Role::Admin)),
            "only the owner and admins can rename other machines; run this on {}",
            self.deciders(&snap)
        );
        let allowed = |r: &Roster| match caller {
            Caller::Member(by) if by == id => {
                ensure!(r.is_member(&by), "a machine outside the cluster cannot rename itself");
                Ok(())
            }
            _ => caller_may(r, caller, decides, "rename another machine"),
        };
        self.store.sign_if(self.endpoint.secret_key(), allowed, |_| Change::Rename { id, name: name.to_string() })
    }

    /// Makes plain member `id` an admin. Only the owner may.
    pub fn promote(&self, id: EndpointId) -> Result<()> {
        let snap = self.store.snapshot();
        ensure!(snap.role(&id) == Some(Role::Member), "{} is not a plain member", id.fmt_short());
        ensure!(self.role() == Some(Role::Owner), "only the owner can promote; run this on {}", self.owner_name(&snap));
        self.store.sign(self.endpoint.secret_key(), Change::Promote { id })
    }

    fn owner_name(&self, snap: &crate::state::Snapshot) -> String {
        if self.cut_off.load(std::sync::atomic::Ordering::Relaxed) {
            return REMOVED.into();
        }
        snap.members.values().find(|m| m.role == Role::Owner).map_or_else(|| "the owner".into(), |m| m.name.clone())
    }

    /// Makes this machine the owner with the recovery code. The root key is used once and kept
    /// nowhere.
    pub fn recover(&self, code: &str) -> Result<()> {
        let root = root_from_code(code)?;
        let snap = self.store.snapshot();
        let cluster = snap.cluster.as_ref().context("machine is not in a cluster")?;
        ensure!(root.public().to_string() == cluster.id, "this recovery code is for another cluster");
        let name = snap.members.get(&self.id()).context("machine is not a member")?.name.clone();
        self.store.recover(&root, self.endpoint.secret_key(), name)
    }

    /// Every member, with its info and how the last hello with it went. With `probe`, says hello
    /// to every member first.
    pub async fn machines(&self, probe: bool) -> Vec<MachineRow> {
        if probe {
            self.hello_all().await;
        }
        let snap = self.store.snapshot();
        snap.members
            .iter()
            .map(|(id, m)| MachineRow {
                id: *id,
                name: m.name.clone(),
                role: m.role,
                info: snap.machine(id),
                status: self.peer_status(id),
            })
            .collect()
    }

    /// Makes every machine use exactly these relays. No relays: each machine goes back to the
    /// relays it started with. Only the owner and admins may.
    pub async fn set_cluster_relays(&self, caller: Caller, settings: &RelaySettings) -> Result<()> {
        let snap = self.store.snapshot();
        ensure!(
            matches!(self.role(), Some(Role::Owner | Role::Admin)),
            "only the owner and admins can set the cluster relays; run this on {}",
            self.deciders(&snap)
        );
        let relays =
            Relays { urls: settings.relays.iter().map(|u| u.to_string()).collect(), token: settings.token.clone() };
        let allowed = |r: &Roster| caller_may(r, caller, decides, "set the cluster relays");
        self.store.sign_if(self.endpoint.secret_key(), allowed, |_| Change::Relays(relays))
    }

    /// Asks the owner of the machine to reset it: a new key, out of any cluster, with its data
    /// deleted. [`super::Running::reset_requested`] resolves; [`super::serve`] does the rest.
    pub fn request_reset(&self) {
        self.reset.send_replace(true);
    }

    /// Resets after the reply to the current API call has left: the API handler calls
    /// [`Machine::finish_reset`] then. A fallback timer resets anyway.
    /// Resets this machine for `caller` once the reply has left: a new key, out of any cluster,
    /// with its data deleted. A remote caller may only where it could remove this machine. With
    /// `if_alone`, only while [`Machine::alone`] holds, so a caller that saw it alone loses
    /// nothing that came after. The checks and the decision run under the roster lock, so no
    /// roster change lands between them.
    pub fn reset(&self, caller: Caller, if_alone: bool) -> Result<()> {
        self.store.locked(|r| {
            if let Caller::Member(_) = caller {
                let r = r.context("machine is not in a cluster")?;
                let theirs = r.role(&self.id());
                let may =
                    |mine| mine == Some(Role::Owner) || (mine == Some(Role::Admin) && theirs == Some(Role::Member));
                caller_may(r, caller, may, "reset this machine")?;
            }
            ensure!(!if_alone || self.alone(), "the machine has other members or app files now; it was not reset");
            self.reset_after_reply();
            Ok(())
        })
    }

    fn reset_after_reply(&self) {
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
        for ok in ["mac", "dev-box_1"] {
            valid_name(ok).unwrap();
        }
        for bad in ["", "self", "SELF", "a b", "a/b", "a.b", "a@b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_cluster_has_this_machine_alone_as_owner() {
        let dir = tempfile::tempdir().unwrap();
        let running = Builder::new(Config::new(dir.path())).spawn().await.unwrap();
        let m = running.machine.clone();
        assert!(!m.alone(), "in no cluster");
        let init = m.init_cluster(Some("one".into())).await.unwrap();
        assert!(m.alone(), "alone");
        std::fs::create_dir_all(dir.path().join("app")).unwrap();
        assert!(m.alone(), "an empty app directory holds nothing");
        std::fs::write(dir.path().join("app/data"), "x").unwrap();
        assert!(!m.alone(), "a reset would delete the app files");
        assert!(m.reset(Caller::Local, true).is_err(), "a caller that saw it alone resets nothing now");
        assert_eq!(m.role(), Some(Role::Owner));
        assert_eq!(root_from_code(&init.recovery_code).unwrap().public().to_string(), init.cluster);
        running.shutdown().await.unwrap();
    }

    /// Two machines in one cluster, with b's copy taken from the ticket, as a join takes it, and
    /// no hello between them.
    async fn pair(dir: &std::path::Path) -> (super::super::Running, super::super::Running) {
        let a = Builder::new(Config::new(dir.join("a"))).spawn().await.unwrap();
        let b = Builder::new(Config::new(dir.join("b"))).spawn().await.unwrap();
        a.machine.init_cluster(Some("a".into())).await.unwrap();
        let ticket = a.machine.admit(Caller::Local, b.machine.id(), "b", None).await.unwrap();
        b.machine.store.merge(ticket.roster, Some(a.machine.store.roster().unwrap().network)).await.unwrap();
        (a, b)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_caller_acts_through_a_machine_only_with_its_own_role() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = pair(dir.path()).await;
        let (m, b_id) = (a.machine.clone(), Caller::Member(b.machine.id()));
        let other = iroh::SecretKey::generate().public();
        assert!(m.admit(b_id, other, "other", None).await.is_err(), "a plain member cannot add");
        assert!(m.set_cluster_relays(b_id, &RelaySettings::default()).await.is_err());
        m.rename_member(b_id, b.machine.id(), "bee").unwrap();
        assert!(m.rename_member(b_id, m.id(), "aye").is_err(), "a plain member renames only itself");
        assert!(m.reset(b_id, false).is_err(), "a plain member cannot reset the owner");
        assert!(m.reset(Caller::Local, true).is_err(), "a has a member, so it is not alone");
        m.promote(b.machine.id()).unwrap();
        m.admit(b_id, other, "other", None).await.unwrap();
        // Once removed, b acts through a no more, also when it passed a check before.
        m.remove_member(Caller::Local, b.machine.id(), true).await.unwrap();
        let next = iroh::SecretKey::generate().public();
        assert!(m.admit(b_id, next, "next", None).await.is_err());
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_loopback_relay_of_another_machine_is_dropped_unless_shared() {
        let dir = tempfile::tempdir().unwrap();
        let running = Builder::new(Config::new(dir.path())).spawn().await.unwrap();
        let m = running.machine.clone();
        m.init_cluster(Some("one".into())).await.unwrap();
        let (lan, local, public): (std::net::SocketAddr, iroh::RelayUrl, iroh::RelayUrl) = (
            "192.0.2.1:5".parse().unwrap(),
            "http://127.0.0.1:4000/".parse().unwrap(),
            "https://relay.example/".parse().unwrap(),
        );
        let peer = iroh::SecretKey::generate().public();
        let published = iroh::EndpointAddr::from_parts(
            peer,
            [
                iroh::TransportAddr::Ip(lan),
                iroh::TransportAddr::Relay(local.clone()),
                iroh::TransportAddr::Relay(public.clone()),
            ],
        );
        let addr = m.reachable(published.clone());
        assert!(addr.ip_addrs().any(|a| *a == lan));
        assert!(addr.relay_urls().any(|u| *u == public));
        assert!(!addr.relay_urls().any(|u| *u == local), "the peer's own loopback relay is no use here");
        let shared = RelaySettings { relays: vec![local.clone()], token: None };
        m.set_cluster_relays(Caller::Local, &shared).await.unwrap();
        assert!(m.reachable(published).relay_urls().any(|u| *u == local), "a shared loopback relay stays");
        running.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_the_devtunnel_mesh_a_loopback_relay_of_another_machine_is_always_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::new(dir.path());
        config.net.devtunnel_mesh = true;
        let running = Builder::new(config).spawn().await.unwrap();
        let m = running.machine.clone();
        m.init_cluster(Some("one".into())).await.unwrap();
        let (local, public): (iroh::RelayUrl, iroh::RelayUrl) =
            ("http://127.0.0.1:4000/".parse().unwrap(), "https://relay.example/".parse().unwrap());
        let published = iroh::EndpointAddr::from_parts(
            iroh::SecretKey::generate().public(),
            [iroh::TransportAddr::Relay(local.clone()), iroh::TransportAddr::Relay(public.clone())],
        );
        let addr = m.reachable(published.clone());
        assert!(addr.relay_urls().any(|u| *u == public), "a public relay stays");
        assert!(!addr.relay_urls().any(|u| *u == local), "this machine reaches the peer through its own forward");
        let initial = RelaySettings { relays: vec![local.clone()], token: None };
        m.set_cluster_relays(Caller::Local, &initial).await.unwrap();
        let again = m.reachable(published);
        assert_eq!(
            again.relay_urls().filter(|u| **u == local).count(),
            1,
            "the cluster relay is added once, as a cluster relay"
        );
        running.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leave_that_no_member_answered_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = pair(dir.path()).await;
        a.shutdown().await.unwrap();
        for _ in 0..2 {
            let e = b.machine.leave().await.unwrap_err().to_string();
            assert!(e.contains("no other member answered"), "{e}");
        }
        assert!(b.machine.store.read(|s| s.is_removed(&b.machine.id())));
        b.shutdown().await.unwrap();
    }
}
