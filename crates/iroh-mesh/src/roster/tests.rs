use super::*;
use rand::rngs::StdRng;
use rand::seq::{IndexedRandom, SliceRandom};
use rand::{Rng, SeedableRng};

fn key() -> SecretKey {
    SecretKey::generate()
}

fn admit(k: &SecretKey, name: &str) -> Change {
    Change::Admit { id: k.public(), name: name.into() }
}

fn promote(k: &SecretKey) -> Change {
    Change::Promote { id: k.public() }
}

fn at_port(k: &SecretKey, port: u16) -> Change {
    Change::Locate(Location::new(EndpointAddr::new(k.public()).with_ip_addr(([10, 0, 0, 1], port).into())))
}

/// `r` with an entry added as is, as a thief would send it.
fn forged(r: &Roster, key: &SecretKey, at: u64, change: Change) -> Roster {
    let mut copy = r.clone();
    copy.log.insert(Signed::new(&r.network, key, at, change));
    copy
}

fn size(r: &Roster) -> usize {
    serde_json::to_vec(r).unwrap().len()
}

/// desk owns the cluster, server is an admin, and laptop is a plain member that server admitted.
struct Cluster {
    r: Roster,
    desk: SecretKey,
    server: SecretKey,
    laptop: SecretKey,
}

fn cluster() -> Cluster {
    let (desk, server, laptop) = (key(), key(), key());
    let (mut r, _root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&server, "server")).unwrap();
    r.sign(&desk, 3, promote(&server)).unwrap();
    r.sign(&server, 4, admit(&laptop, "laptop")).unwrap();
    Cluster { r, desk, server, laptop }
}

#[test]
fn the_owner_admits_admins_admit_members_cannot() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    let phone = key();
    assert_eq!(r.role(&desk.public()), Some(Role::Owner));
    assert_eq!(r.role(&server.public()), Some(Role::Admin));
    assert_eq!(r.role(&laptop.public()), Some(Role::Member));
    assert!(r.sign(&laptop, 5, admit(&phone, "phone")).is_err());
    assert!(r.sign(&laptop, 5, r.removal(server.public(), false)).is_err());
    assert!(r.sign(&server, 6, r.removal(desk.public(), false)).is_err());
    assert!(r.sign(&server, 6, promote(&laptop)).is_err());
    r.sign(&server, 7, r.removal(laptop.public(), false)).unwrap();
    assert!(!r.is_member(&laptop.public()) && r.is_removed(&laptop.public()));
    r.sign(&server, 8, admit(&phone, "phone")).unwrap();
    assert_eq!(r.role(&phone.public()), Some(Role::Member));
}

#[test]
fn copies_converge_whatever_the_order() {
    let (desk, server, laptop, phone) = (key(), key(), key(), key());
    let (mut a, _root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    a.sign(&desk, 2, admit(&server, "server")).unwrap();
    let mut c = a.clone();
    a.sign(&desk, 3, promote(&server)).unwrap();
    let mut b = a.clone();
    a.sign(&desk, 4, admit(&laptop, "laptop")).unwrap();
    b.sign(&server, 4, admit(&phone, "phone")).unwrap();
    // c knows neither admission, nor that server is an admin. It hears of b first, then a.
    assert!(c.merge(&b) && c.is_member(&phone.public()));
    assert!(c.merge(&a));
    assert!(a.merge(&b) && b.merge(&a));
    assert_eq!((a.members(), b.members()), (c.members(), c.members()));
    assert_eq!(a.members().len(), 4);
    assert_eq!((a.digest(), b.digest()), (c.digest(), c.digest()));
    assert!(!a.merge(&b), "nothing new the second time");
}

#[test]
fn forged_or_foreign_entries_are_dropped() {
    let (desk, mallory) = (key(), key());
    let (mut r, _root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    let mut forged = r.clone();
    let mut entry = Signed::new(&r.network, &mallory, 2, admit(&mallory, "m"));
    entry.by = desk.public();
    forged.log.insert(entry);
    assert!(!r.merge(&forged));
    let (other, _) = Roster::create(&mallory, "m".into(), 1).unwrap();
    assert!(!r.merge(&other));
    // An entry signed for another network does not verify here.
    let mut foreign = r.clone();
    foreign.log.insert(Signed::new(&other.network, &desk, 2, admit(&mallory, "m")));
    assert!(!r.merge(&foreign));
    assert_eq!(r.members().len(), 1);
}

#[test]
fn removing_a_stolen_admin_with_cascade_voids_its_admissions() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    let evil = key();
    r.sign(&server, 5, admit(&evil, "evil")).unwrap();
    let mut retired = r.clone();
    retired.sign(&desk, 6, retired.removal(server.public(), false)).unwrap();
    assert!(retired.is_member(&laptop.public()), "retiring an admin keeps whom it admitted");
    r.sign(&desk, 6, r.removal(server.public(), true)).unwrap();
    assert!(!r.is_member(&evil.public()) && !r.is_member(&laptop.public()));
    assert!(r.is_member(&desk.public()));
    assert_eq!(r.members().len(), 1);
}

#[test]
fn a_removed_admin_cannot_backdate_an_admission() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    r.sign(&desk, 10, r.removal(server.public(), false)).unwrap();
    let evil = key();
    for at in [0, 5, 9, 11] {
        r.merge(&forged(&r, &server, at, admit(&evil, "evil")));
        assert!(!r.is_member(&evil.public()), "an admission at {at} does not count");
    }
    assert!(r.is_member(&laptop.public()));
    assert!(r.sign(&server, 5, admit(&evil, "evil")).is_err());
}

#[test]
fn a_removed_admin_keeps_the_admissions_its_remover_counted() {
    let Cluster { r, desk, server, laptop, .. } = cluster();
    let phone = key();
    // server admits phone while desk removes server, before their copies meet.
    let mut at_server = r.clone();
    at_server.sign(&server, 5, admit(&phone, "phone")).unwrap();
    let mut at_desk = r.clone();
    at_desk.sign(&desk, 6, at_desk.removal(server.public(), false)).unwrap();
    at_server.merge(&at_desk);
    at_desk.merge(&at_server);
    assert_eq!(at_server.members(), at_desk.members());
    assert!(at_desk.is_member(&laptop.public()));
    assert!(!at_desk.is_member(&phone.public()), "desk did not count phone's admission");

    // Two removals of one admin: a decision counts only when both list it.
    let mut a = r.clone();
    a.sign(&server, 5, admit(&phone, "phone")).unwrap();
    a.sign(&server, 6, a.removal(server.public(), false)).unwrap();
    assert!(a.is_member(&phone.public()), "server leaves and keeps what it admitted");
    let mut b = r.clone();
    b.sign(&desk, 7, b.removal(server.public(), false)).unwrap();
    a.merge(&b);
    b.merge(&a);
    assert_eq!(a.members(), b.members());
    assert!(a.is_member(&laptop.public()) && !a.is_member(&phone.public()));
}

#[test]
fn the_recovery_code_moves_the_owner() {
    let (desk, laptop) = (key(), key());
    let (mut r, root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&laptop, "laptop")).unwrap();
    let code = recovery_code(&root);
    assert_eq!(code.len(), 64 + 7);
    let root = root_from_code(&code).unwrap();
    r.sign(&root, 3, Change::Owner { id: laptop.public(), name: "laptop".into() }).unwrap();
    assert!(r.sign(&desk, 4, r.removal(laptop.public(), false)).is_err(), "only the root removes the owner");
    r.sign(&laptop, 4, r.removal(desk.public(), false)).unwrap();
    assert_eq!(r.role(&laptop.public()), Some(Role::Owner));
    assert!(!r.is_member(&desk.public()));
    assert!(root_from_code("nope").is_err());
}

#[test]
fn a_stolen_owner_cannot_undo_its_replacement() {
    let (desk, laptop, server) = (key(), key(), key());
    let (mut r, root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&laptop, "laptop")).unwrap();
    r.sign(&root, 10, Change::Owner { id: laptop.public(), name: "laptop".into() }).unwrap();
    let mut retired = r.clone();
    retired.sign(&laptop, 11, retired.removal(desk.public(), false)).unwrap();
    r.sign(&laptop, 11, r.removal(desk.public(), true)).unwrap();
    r.sign(&laptop, 12, admit(&server, "server")).unwrap();
    retired.sign(&laptop, 12, admit(&server, "server")).unwrap();
    // The thief signs as desk, with a time from before the handover.
    let forge = |copy: &Roster| {
        let mut forged = copy.clone();
        let changes = [
            Change::Remove { id: laptop.public(), cascade: true, kept: BTreeSet::new() },
            Change::Remove { id: server.public(), cascade: false, kept: BTreeSet::new() },
            promote(&server),
            Change::Owner { id: desk.public(), name: "desk".into() },
        ];
        forged.log.extend(changes.map(|change| Signed::new(&copy.network, &desk, 9, change)));
        forged
    };
    for copy in [&mut r, &mut retired] {
        copy.merge(&forge(copy));
        assert_eq!(copy.role(&laptop.public()), Some(Role::Owner));
        assert_eq!(copy.role(&server.public()), Some(Role::Member));
        assert!(copy.is_removed(&desk.public()));
    }
}

#[test]
fn admins_cannot_remove_each_other() {
    let Cluster { mut r, desk, server, .. } = cluster();
    let other = key();
    r.sign(&desk, 5, admit(&other, "other")).unwrap();
    r.sign(&desk, 6, promote(&other)).unwrap();
    assert!(r.sign(&server, 7, r.removal(other.public(), true)).is_err());
    assert!(r.sign(&other, 7, r.removal(server.public(), true)).is_err());
    let both =
        forged(&forged(&r, &server, 7, r.removal(other.public(), true)), &other, 7, r.removal(server.public(), true));
    r.merge(&both);
    assert_eq!((r.role(&server.public()), r.role(&other.public())), (Some(Role::Admin), Some(Role::Admin)));
}

#[test]
fn a_machine_can_remove_itself() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    r.sign(&laptop, 5, r.removal(laptop.public(), false)).unwrap();
    assert!(r.is_removed(&laptop.public()));
    assert!(r.sign(&laptop, 6, r.removal(laptop.public(), false)).is_err(), "it left already");
    let phone = key();
    r.sign(&server, 6, admit(&phone, "phone")).unwrap();
    r.sign(&server, 7, r.removal(server.public(), false)).unwrap();
    assert!(
        r.is_removed(&server.public()) && r.is_member(&phone.public()),
        "an admin that leaves keeps its admissions"
    );
    assert!(r.sign(&desk, 8, r.removal(desk.public(), false)).is_err(), "the owner cannot leave");
}

#[test]
fn a_machine_moves_itself_only() {
    let Cluster { mut r, desk, laptop, .. } = cluster();
    r.sign(&laptop, 10, at_port(&laptop, 1)).unwrap();
    assert!(r.sign(&desk, 11, at_port(&laptop, 666)).is_err());
    let Change::Locate(far) = at_port(&laptop, 2) else { unreachable!() };
    r.sign(&laptop, 1_000, Change::Locate(far.clone())).unwrap();
    assert!(r.sign(&laptop, 20, at_port(&laptop, 3)).is_err(), "an older location does not count");
    assert_eq!(r.locate(&laptop.public()), Some(&far));
    // Once the clock passes a location from the future, the next one wins.
    r.sign(&laptop, 1_001, at_port(&laptop, 4)).unwrap();
    assert_eq!(r.locate(&laptop.public()).unwrap().addr.ip_addrs().next().unwrap().port(), 4);
    assert_eq!(r.log.iter().filter(|s| matches!(s.change, Change::Locate(_))).count(), 1);
    // A removed machine keeps no location.
    r.sign(&desk, 1_002, r.removal(laptop.public(), false)).unwrap();
    assert!(r.locate(&laptop.public()).is_none());
    assert!(!r.log.iter().any(|s| matches!(s.change, Change::Locate(_))));
}

#[test]
fn a_stolen_member_cannot_grow_the_log() {
    let Cluster { mut r, laptop, .. } = cluster();
    let before = size(&r);
    let mut junk = r.clone();
    for at in 5..2_000 {
        let stranger = key();
        let changes = [
            at_port(&laptop, at as u16),
            admit(&stranger, "x"),
            promote(&stranger),
            Change::Remove { id: laptop.public(), cascade: false, kept: BTreeSet::new() },
            Change::Relays(Relays { urls: vec![format!("https://relay{at}.example")], token: None }),
        ];
        junk.log.extend(changes.map(|change| Signed::new(&r.network, &laptop, at, change)));
        junk.log.insert(Signed::new(&r.network, &stranger, at, at_port(&stranger, 1)));
    }
    assert!(r.merge(&junk));
    assert_eq!(r.members().len(), 2);
    assert!(r.is_removed(&laptop.public()), "only laptop's first leave stays");
    assert!(r.log.iter().filter(|s| s.by == laptop.public()).count() <= MAX_PER_SIGNER);
    assert!(size(&r) < before + 1024, "the log stays as small as it was");
}

#[test]
fn entries_out_of_bounds_dont_count() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    let before = size(&r);
    let huge = (0..1_000u32).fold(EndpointAddr::new(laptop.public()), |a, n| {
        let [_, b, c, d] = n.to_be_bytes();
        a.with_ip_addr(std::net::SocketAddr::from(([10, b, c, d], 1)))
    });
    let mut big = Location::new(EndpointAddr::new(laptop.public()));
    big.labels.insert("note".into(), "x".repeat(MAX_ENTRY));
    let too_many: BTreeSet<Digest> =
        (0..=MAX_PER_SIGNER as u32).map(|n| Digest(blake3::hash(&n.to_be_bytes()).into())).collect();
    let stranger = key();
    let out = [
        (&laptop, Change::Locate(Location { addr: huge.clone(), ..Location::new(huge.clone()) })),
        (&laptop, Change::Locate(big)),
        (&server, admit(&stranger, &"x".repeat(MAX_NAME + 1))),
        (&server, admit(&stranger, "a.b")),
        (&server, admit(&stranger, "a b")),
        (&server, admit(&stranger, "a@b")),
        (&server, admit(&stranger, "a/b")),
        (&server, admit(&stranger, "")),
        (&desk, Change::Remove { id: stranger.public(), cascade: false, kept: too_many }),
    ];
    for (signer, change) in out {
        assert!(r.sign(signer, 5, change.clone()).is_err(), "{change:?} is refused");
        assert!(!r.merge(&forged(&r, signer, 5, change)), "nothing out of bounds counts");
    }
    assert_eq!(r.members().len(), 3);
    assert!(!r.is_removed(&stranger.public()));
    assert_eq!(size(&r), before);
    // A location cut to its bounds counts.
    r.sign(&laptop, 6, Change::Locate(Location::new(huge))).unwrap();
    assert_eq!(r.locate(&laptop.public()).unwrap().addr.addrs.len(), MAX_ADDRS);
    r.sign(&server, 7, admit(&stranger, &"x".repeat(MAX_NAME))).unwrap();
}

#[test]
fn two_admins_naming_a_machine_at_once_get_distinct_names() {
    let Cluster { r: mut a, desk, server, .. } = cluster();
    let (first, second) = (key(), key());
    let mut b = a.clone();
    a.sign(&desk, 5, admit(&first, "box")).unwrap();
    b.sign(&server, 6, admit(&second, "Box")).unwrap();
    a.merge(&b);
    b.merge(&a);
    assert_eq!(a.members(), b.members());
    assert_eq!((a.name(&first.public()), a.name(&second.public())), (Some("box"), Some("Box-2")));
    // Removing the first moves no name. A machine admitted later may take the freed name.
    a.sign(&desk, 7, a.removal(first.public(), false)).unwrap();
    assert_eq!(a.name(&second.public()), Some("Box-2"));
    let third = key();
    a.sign(&desk, 8, admit(&third, "box")).unwrap();
    assert_eq!(a.name(&third.public()), Some("box"));
}

#[test]
fn the_newest_relay_setting_by_the_owner_or_an_admin_wins() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    let set = |url: &str| Change::Relays(Relays { urls: vec![url.into()], token: Some("t".into()) });
    assert!(r.sign(&laptop, 5, set("https://member.example")).is_err());
    r.sign(&desk, 5, set("https://desk.example")).unwrap();
    r.sign(&server, 6, set("https://server.example")).unwrap();
    assert!(r.sign(&desk, 4, set("https://old.example")).is_err());
    assert_eq!(r.relays().unwrap().urls, ["https://server.example"]);
    r.sign(&desk, 7, r.removal(server.public(), false)).unwrap();
    assert_eq!(r.relays().unwrap().urls, ["https://desk.example"], "a removed admin sets no relays");
}

#[test]
fn a_machine_renames_itself_and_admins_rename_anyone() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    let rename = |k: &SecretKey, name: &str| Change::Rename { id: k.public(), name: name.into() };
    r.sign(&laptop, 5, rename(&laptop, "notebook")).unwrap();
    assert_eq!(r.name(&laptop.public()), Some("notebook"));
    assert!(r.sign(&laptop, 6, rename(&server, "mine")).is_err(), "a plain member renames only itself");
    r.sign(&server, 7, rename(&laptop, "lap")).unwrap();
    assert_eq!(r.name(&laptop.public()), Some("lap"));
    r.sign(&desk, 8, rename(&laptop, "desk")).unwrap();
    assert_eq!(r.name(&laptop.public()), Some("desk-2"), "a taken name gets a number");
    let before = size(&r);
    for n in 0..50 {
        r.sign(&laptop, 9 + n, rename(&laptop, &format!("n{n}"))).unwrap();
    }
    assert!(size(&r) < before + 1024, "only the newest rename by each signer stays");
}

#[test]
fn an_admin_that_leaves_keeps_its_admissions() {
    let Cluster { mut r, server, laptop, .. } = cluster();
    r.sign(&server, 5, r.removal(server.public(), false)).unwrap();
    assert!(r.is_member(&laptop.public()), "an admin that leaves keeps its admissions");
    assert!(!r.is_member(&server.public()));
}

#[test]
fn a_replaced_owner_cannot_backdate_a_decision() {
    let (desk, laptop, x) = (key(), key(), key());
    let (mut r, root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&laptop, "laptop")).unwrap();
    r.sign(&root, 10, Change::Owner { id: laptop.public(), name: "laptop".into() }).unwrap();
    r.sign(&laptop, 11, r.removal(desk.public(), false)).unwrap();
    assert!(r.is_member(&laptop.public()), "the replaced owner's earlier admission keeps counting");
    r.merge(&forged(&r, &desk, 9, admit(&x, "x")));
    assert!(!r.is_member(&x.public()), "a decision dated before the handover but not in it does not count");
}

#[test]
fn junk_from_a_stolen_admin_evicts_no_one() {
    let Cluster { mut r, desk, server, .. } = cluster();
    let (a2, phone) = (key(), key());
    r.sign(&desk, 5, admit(&a2, "a2")).unwrap();
    r.sign(&desk, 6, promote(&a2)).unwrap();
    r.sign(&a2, 7, admit(&phone, "phone")).unwrap();
    let mut junk = r.clone();
    for i in 0..100u64 {
        let kept = (0..1024u64).map(|j| Digest(hash(&[i.to_le_bytes(), j.to_le_bytes()].concat()))).collect();
        junk.log.insert(Signed::new(
            &r.network,
            &server,
            0,
            Change::Remove { id: key().public(), cascade: false, kept },
        ));
    }
    r.merge(&junk);
    assert!(r.is_member(&phone.public()), "junk removals of unknown machines evict no one");
    assert!(size(&r) <= MAX_LOG, "the log stays in bounds");
    r.sign(&desk, 8, r.removal(server.public(), true)).unwrap();
    assert!(r.is_member(&phone.public()));
}

#[test]
fn a_rename_cannot_take_a_held_name() {
    let Cluster { mut r, desk, laptop, .. } = cluster();
    let phone = key();
    r.sign(&desk, 5, admit(&phone, "phone")).unwrap();
    r.merge(&forged(&r, &laptop, 6, Change::Rename { id: laptop.public(), name: "phone".into() }));
    assert_eq!(r.name(&phone.public()), Some("phone"));
    assert_eq!(r.name(&laptop.public()), Some("phone-2"));
}

#[test]
fn names_with_control_characters_are_refused() {
    let Cluster { mut r, desk, .. } = cluster();
    for bad in ["a\nHost evil", "tab\there", "esc\u{1b}[31m", "a;b"] {
        assert!(r.sign(&desk, 5, admit(&key(), bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn a_stolen_admin_cannot_void_its_admissions_by_leaving() {
    let Cluster { mut r, server, laptop, .. } = cluster();
    let leave = Change::Remove { id: server.public(), cascade: true, kept: BTreeSet::new() };
    r.merge(&forged(&r, &server, 0, leave));
    assert!(r.is_member(&laptop.public()), "a leave voids nothing");
    assert!(r.unbounded().contains(&server.public()), "the owner must still bound what server decided");
}

#[test]
fn a_replaced_owner_cannot_void_its_admissions_by_leaving() {
    let (desk, laptop, server) = (key(), key(), key());
    let (mut r, root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&laptop, "laptop")).unwrap();
    r.sign(&desk, 3, admit(&server, "server")).unwrap();
    r.sign(&root, 10, Change::Owner { id: laptop.public(), name: "laptop".into() }).unwrap();
    assert!(r.unbounded().contains(&desk.public()), "a former owner is unbounded until removed");
    r.sign(&laptop, 11, r.removal(desk.public(), false)).unwrap();
    assert!(r.unbounded().is_empty());
    r.merge(&forged(&r, &desk, 5, Change::Remove { id: desk.public(), cascade: true, kept: BTreeSet::new() }));
    assert!(r.is_member(&server.public()));
}

#[test]
fn the_owners_removal_bounds_an_admin_that_left() {
    let Cluster { mut r, desk, server, laptop, .. } = cluster();
    r.sign(&server, 5, r.removal(server.public(), false)).unwrap();
    r.sign(&desk, 6, r.removal(server.public(), false)).unwrap();
    assert!(r.unbounded().is_empty());
    let late = key();
    r.merge(&forged(&r, &server, 4, admit(&late, "late")));
    assert!(!r.is_member(&late.public()), "after the owner's removal, a backdated admission does not count");
    assert!(r.is_member(&laptop.public()));
}

#[test]
fn a_saved_roster_loads_back() {
    let Cluster { mut r, laptop, .. } = cluster();
    r.sign(&laptop, 5, at_port(&laptop, 7)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert!(Roster::load(dir.path()).unwrap().is_none());
    r.save(dir.path()).unwrap();
    let back = Roster::load(dir.path()).unwrap().unwrap();
    assert_eq!(back, r);
    assert_eq!((back.members(), back.locate(&laptop.public())), (r.members(), r.locate(&laptop.public())));
    std::fs::write(dir.path().join(FILE), "{").unwrap();
    assert!(Roster::load(dir.path()).is_err());
}

/// `merge` without the signature check, which costs most of the time in a debug build. Every
/// entry in these copies comes from `Signed::new`.
fn absorb(into: &mut Roster, from: &Roster) {
    into.log.extend(from.log.iter().cloned());
    into.rebuild();
}

/// Machines that sign, forge, and merge at random, from one seed.
struct World {
    rng: StdRng,
    root: SecretKey,
    keys: Vec<SecretKey>,
    copies: Vec<Option<Roster>>,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let root = SecretKey::from_bytes(&rng.random());
        let keys: Vec<SecretKey> = (0..6).map(|_| SecretKey::from_bytes(&rng.random())).collect();
        let mut copies = vec![None; keys.len()];
        copies[0] = Some(Roster::founded(&root, &keys[0], "m".into(), rng.random_range(0..50)).unwrap());
        Self { rng, root, keys, copies }
    }

    fn step(&mut self) {
        let holders: Vec<usize> = (0..self.keys.len()).filter(|&i| self.copies[i].is_some()).collect();
        let i = *holders.choose(&mut self.rng).unwrap();
        let target = self.keys[self.rng.random_range(0..self.keys.len())].public();
        let at = self.rng.random_range(0..100);
        let mut copy = self.copies[i].take().unwrap();
        let signer = if self.rng.random_bool(0.1) { &self.root } else { &self.keys[i] };
        let name = ["m", "n", "M"][self.rng.random_range(0..3)].to_string();
        let change = match self.rng.random_range(0..8) {
            0 | 1 => Change::Admit { id: target, name },
            2 => Change::Promote { id: target },
            3 => {
                let Change::Remove { id, cascade, mut kept } = copy.removal(target, self.rng.random_bool(0.3)) else {
                    unreachable!()
                };
                kept.retain(|_| self.rng.random_bool(0.8));
                Change::Remove { id, cascade, kept }
            }
            4 => Change::Owner { id: target, name },
            5 => Change::Locate(Location::new(
                EndpointAddr::new(signer.public()).with_ip_addr(([10, 0, 0, 1], at as u16).into()),
            )),
            6 => Change::Rename { id: target, name },
            _ => Change::Relays(Relays { urls: vec![format!("https://r{at}.example")], token: None }),
        };
        if self.rng.random_bool(0.3) {
            copy.log.insert(Signed::new(&copy.network, signer, at, change));
            copy.rebuild();
        } else {
            let _ = copy.sign(signer, at, change);
        }
        self.copies[i] = Some(copy);
        let j = self.rng.random_range(0..self.keys.len());
        if self.rng.random_bool(0.5) && i != j {
            let from = self.copies[i].clone().unwrap();
            absorb(self.copies[j].get_or_insert_with(|| Roster::empty(from.network)), &from);
        }
    }

    /// What must not depend on merge order. Locations are left out: one that arrives before its
    /// signer's admission is dropped, and its machine sends it again in its next hello.
    fn summary(r: &Roster) -> (BTreeMap<EndpointId, Member>, BTreeSet<EndpointId>, Option<Relays>) {
        (r.members().clone(), r.view.removed.clone(), r.relays().cloned())
    }
}

#[test]
fn random_merge_orders_give_the_same_members() {
    // Fewer cases in a debug build, where one case takes about 5 ms.
    let cases = if cfg!(debug_assertions) { 2_000 } else { 10_000 };
    for seed in 0..cases {
        let mut world = World::new(seed);
        for _ in 0..30 {
            world.step();
        }
        let copies: Vec<Roster> = world.copies.iter().flatten().cloned().collect();
        let network = copies[0].network;
        let mut reference = Roster::empty(network);
        for c in &copies {
            absorb(&mut reference, c);
        }
        let want = World::summary(&reference);
        let rng = &mut world.rng;
        // All copies, merged pairwise at random until one is left.
        let mut pool = copies.clone();
        pool.shuffle(rng);
        while pool.len() > 1 {
            let from = pool.swap_remove(rng.random_range(0..pool.len()));
            let into = rng.random_range(0..pool.len());
            absorb(&mut pool[into], &from);
        }
        assert_eq!(World::summary(&pool[0]), want, "seed {seed}");
        // A random subset of copies, merged in two random orders.
        let mut subset: Vec<&Roster> = copies.iter().filter(|_| rng.random_bool(0.5)).collect();
        let mut results = Vec::new();
        for _ in 0..2 {
            subset.shuffle(rng);
            let mut r = Roster::empty(network);
            for c in &subset {
                absorb(&mut r, c);
            }
            results.push(World::summary(&r));
        }
        assert_eq!(results[0], results[1], "seed {seed}");
    }
}

#[test]
fn junk_from_a_removed_admin_cannot_push_out_what_its_removal_kept() {
    let Cluster { mut r, desk, server, laptop } = cluster();
    r.sign(&desk, 10, r.removal(server.public(), false)).unwrap();
    assert!(r.is_member(&laptop.public()));
    let mut attack = r.clone();
    for n in 0..MAX_PER_SIGNER {
        attack.log.insert(Signed::new(&r.network, &server, 0, admit(&key(), &format!("junk{n}"))));
    }
    // A copy that holds the admission already, and a copy that gets it only with the junk.
    let mut fresh = Roster::empty(r.network);
    fresh.merge(&attack);
    r.merge(&attack);
    for copy in [&r, &fresh] {
        assert!(copy.is_removed(&server.public()));
        assert!(copy.is_member(&laptop.public()));
    }
}

#[test]
fn a_full_signer_share_gives_the_same_members_in_any_merge_order() {
    let Cluster { r: base, desk, server, laptop } = cluster();
    let z = key();
    let mut j = base.clone();
    for at in 10..10 + MAX_PER_SIGNER as u64 {
        let change = Change::Remove { id: z.public(), cascade: false, kept: BTreeSet::new() };
        j.log.insert(Signed::new(&base.network, &server, at, change));
    }
    let mut d = base.clone();
    d.sign(&server, 2000, d.removal(laptop.public(), false)).unwrap();
    let mut t = base.clone();
    t.sign(&desk, 5, admit(&z, "z")).unwrap();
    let mut a = Roster::empty(base.network);
    for copy in [&j, &d, &t] {
        a.merge(copy);
    }
    let mut b = Roster::empty(base.network);
    for copy in [&j, &t, &d] {
        b.merge(copy);
    }
    assert_eq!(a.removed(), b.removed());
    assert_eq!(a.members(), b.members());
}

#[test]
fn an_admin_named_owner_keeps_what_it_decided_as_an_admin() {
    let (desk, server, phone, evil) = (key(), key(), key(), key());
    let (mut r, root) = Roster::create(&desk, "desk".into(), 1).unwrap();
    r.sign(&desk, 2, admit(&server, "server")).unwrap();
    r.sign(&desk, 3, promote(&server)).unwrap();
    r.sign(&desk, 4, admit(&evil, "evil")).unwrap();
    r.sign(&server, 5, admit(&phone, "phone")).unwrap();
    r.sign(&server, 6, r.removal(evil.public(), false)).unwrap();
    r.sign(&root, 10, Change::Owner { id: server.public(), name: "server".into() }).unwrap();
    r.sign(&server, 11, r.removal(desk.public(), false)).unwrap();
    assert_eq!(r.role(&server.public()), Some(Role::Owner));
    assert!(r.is_member(&phone.public()));
    assert!(r.is_removed(&evil.public()));
}

#[test]
fn a_backdated_rename_cannot_take_a_renamed_members_name() {
    let Cluster { mut r, desk, laptop, .. } = cluster();
    let phone = key();
    r.sign(&desk, 5, admit(&phone, "phone")).unwrap();
    r.sign(&phone, 6, Change::Rename { id: phone.public(), name: "workstation".into() }).unwrap();
    assert_eq!(r.name(&phone.public()), Some("workstation"));
    r.merge(&forged(&r, &laptop, 0, Change::Rename { id: laptop.public(), name: "workstation".into() }));
    assert_ne!(r.name(&laptop.public()), Some("workstation"));
}
