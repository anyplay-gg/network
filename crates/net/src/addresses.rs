//! The virtual address each peer has on this PC.
//!
//! The cloud gives every member of a session an address of its own inside that session, and
//! nothing keeps two sessions from giving one address to two different players (ADR 0020). The
//! game and the tunnels tell peers apart by address alone, so this PC keeps one address per
//! peer account over all sessions: the one the cloud named, and when another peer already holds
//! that one, the next free one. A peer keeps its address for as long as it is in any session
//! this PC holds, so the game never sees it change.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use anyplay_api::{Session, VIRTUAL_ADDRESSES, is_assignable, virtual_address, virtual_slot};

#[derive(Debug, Default)]
pub struct PeerAddresses {
    assigned: HashMap<u64, Ipv4Addr>,
}

impl PeerAddresses {
    /// The address of a peer in the sessions seen so far.
    pub fn of(&self, account: u64) -> Option<Ipv4Addr> {
        self.assigned.get(&account).copied()
    }

    /// Takes in the current sessions: peers that left all of them lose their address, new peers
    /// get one. `me` is never given an address here, but its addresses in the sessions are kept
    /// free so that no peer looks like the player.
    pub fn refresh(&mut self, me: Option<u64>, sessions: &HashMap<String, Session>) {
        let mut ordered: Vec<&Session> = sessions.values().collect();
        ordered.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));

        let mut present = HashSet::new();
        let mut own = HashSet::new();
        for session in &ordered {
            for member in &session.members {
                if Some(member.account.id.0) == me {
                    own.extend(member.address.parse::<Ipv4Addr>().ok());
                } else {
                    present.insert(member.account.id.0);
                }
            }
        }
        self.assigned.retain(|account, _| present.contains(account));

        let mut taken: HashSet<Ipv4Addr> = self.assigned.values().copied().collect();
        taken.extend(own);
        for session in &ordered {
            for member in &session.members {
                let account = member.account.id.0;
                if Some(account) == me || self.assigned.contains_key(&account) {
                    continue;
                }
                let wanted = member.address.parse::<Ipv4Addr>().ok();
                if let Some(address) = free_from(wanted, &taken) {
                    taken.insert(address);
                    self.assigned.insert(account, address);
                }
            }
        }
    }
}

/// `wanted` when it can be used and is free, else the next free address after it (from the
/// start of the range when there is no `wanted`). `None` only when the range is used up.
fn free_from(wanted: Option<Ipv4Addr>, taken: &HashSet<Ipv4Addr>) -> Option<Ipv4Addr> {
    let start = wanted.and_then(virtual_slot).unwrap_or(0);
    (0..VIRTUAL_ADDRESSES)
        .filter_map(|step| virtual_address((start + step) % VIRTUAL_ADDRESSES))
        .find(|address| is_assignable(*address) && !taken.contains(address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_api::{Account, AccountId, Member, Visibility};

    fn member(id: u64, address: &str) -> Member {
        Member {
            account: Account {
                id: AccountId(id),
                handle: format!("p{id}"),
            },
            endpoint: String::new(),
            relay: None,
            address: address.into(),
            joined: 0,
        }
    }

    fn session(id: &str, created: u64, members: Vec<Member>) -> Session {
        Session {
            id: id.into(),
            title_id: "GAME00001".into(),
            title: "Game".into(),
            version: "1".into(),
            build: "1".into(),
            leader: members[0].account.id,
            max_members: 8,
            visibility: Visibility::Friends,
            properties: String::new(),
            members,
            created,
            revision: 0,
        }
    }

    fn sessions(list: Vec<Session>) -> HashMap<String, Session> {
        list.into_iter().map(|s| (s.id.clone(), s)).collect()
    }

    fn addr(text: &str) -> Ipv4Addr {
        text.parse().unwrap()
    }

    #[test]
    fn a_peer_gets_the_address_the_cloud_named() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![session(
                "a",
                1,
                vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
            )]),
        );
        assert_eq!(book.of(2), Some(addr("198.18.0.9")));
        assert_eq!(book.of(1), None, "the player has no peer address");
    }

    #[test]
    fn one_peer_in_two_sessions_keeps_one_address() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![
                session(
                    "a",
                    1,
                    vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
                ),
                session(
                    "b",
                    2,
                    vec![member(1, "198.18.0.5"), member(2, "198.18.3.9")],
                ),
            ]),
        );
        assert_eq!(
            book.of(2),
            Some(addr("198.18.0.9")),
            "the older session decides"
        );
    }

    #[test]
    fn two_peers_the_cloud_gave_one_address_get_different_ones() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![
                session(
                    "a",
                    1,
                    vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
                ),
                session(
                    "b",
                    2,
                    vec![member(1, "198.18.0.6"), member(3, "198.18.0.9")],
                ),
            ]),
        );
        assert_eq!(book.of(2), Some(addr("198.18.0.9")));
        assert_eq!(book.of(3), Some(addr("198.18.0.10")));
    }

    #[test]
    fn a_peer_never_takes_the_address_the_player_has_in_another_session() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![
                session(
                    "a",
                    1,
                    vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
                ),
                session(
                    "b",
                    2,
                    vec![member(1, "198.18.0.7"), member(3, "198.18.0.5")],
                ),
            ]),
        );
        assert_eq!(book.of(3), Some(addr("198.18.0.6")));
    }

    #[test]
    fn a_peer_keeps_its_address_when_the_session_that_named_it_ends() {
        let mut book = PeerAddresses::default();
        let a = session(
            "a",
            1,
            vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
        );
        let b = session(
            "b",
            2,
            vec![member(1, "198.18.0.5"), member(2, "198.18.3.9")],
        );
        book.refresh(Some(1), &sessions(vec![a, b.clone()]));
        book.refresh(Some(1), &sessions(vec![b]));
        assert_eq!(book.of(2), Some(addr("198.18.0.9")));
    }

    #[test]
    fn a_peer_that_left_everything_gives_its_address_up() {
        let mut book = PeerAddresses::default();
        let a = session(
            "a",
            1,
            vec![member(1, "198.18.0.5"), member(2, "198.18.0.9")],
        );
        book.refresh(Some(1), &sessions(vec![a]));
        book.refresh(Some(1), &sessions(vec![]));
        assert_eq!(book.of(2), None);
        // It may come back with another address.
        book.refresh(
            Some(1),
            &sessions(vec![session(
                "c",
                3,
                vec![member(1, "198.18.0.5"), member(2, "198.18.4.4")],
            )]),
        );
        assert_eq!(book.of(2), Some(addr("198.18.4.4")));
    }

    #[test]
    fn an_address_outside_the_range_is_replaced() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![session(
                "a",
                1,
                vec![
                    member(1, "198.18.0.5"),
                    member(2, "10.0.0.1"),
                    member(3, ""),
                ],
            )]),
        );
        let (two, three) = (book.of(2).unwrap(), book.of(3).unwrap());
        assert!(is_assignable(two) && is_assignable(three) && two != three);
    }

    #[test]
    fn the_end_of_the_range_wraps_and_skips_block_ends() {
        let mut book = PeerAddresses::default();
        book.refresh(
            Some(1),
            &sessions(vec![
                session(
                    "a",
                    1,
                    vec![member(1, "198.18.0.5"), member(2, "198.19.255.254")],
                ),
                session(
                    "b",
                    2,
                    vec![member(1, "198.18.0.6"), member(3, "198.19.255.254")],
                ),
            ]),
        );
        assert_eq!(book.of(3), Some(addr("198.18.0.1")));
    }
}
