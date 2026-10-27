//! The device's own state (ARCHITECTURE §4.2, ADR-0016): Connected, Paused or Left, from
//! user commands and network changes. A change of network is debounced; an unknown
//! network makes the device leave, a known one with `auto_join` brings it back.
//! Without any saved profile nothing happens automatically.

use std::time::Duration;

use bridge_protocol::control::{DeviceState, StateReason};

use crate::Micros;

pub type NetworkFingerprint = [u8; 32];

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NetworkProfile {
    pub name: String,
    pub fingerprints: Vec<NetworkFingerprint>,
    pub auto_join: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StateCommand {
    Pause,
    Resume,
    Leave,
    Rejoin,
}

pub const NETWORK_DEBOUNCE: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct DeviceStateMachine {
    state: DeviceState,
    reason: StateReason,
    profiles: Vec<NetworkProfile>,
    network: Option<NetworkFingerprint>,
    pending: Option<(Option<NetworkFingerprint>, Micros)>,
}

impl Default for DeviceStateMachine {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl DeviceStateMachine {
    pub fn new(profiles: Vec<NetworkProfile>) -> Self {
        Self {
            state: DeviceState::Connected,
            reason: StateReason::USER,
            profiles,
            network: None,
            pending: None,
        }
    }

    pub fn state(&self) -> (DeviceState, StateReason) {
        (self.state, self.reason)
    }

    pub fn profiles(&self) -> &[NetworkProfile] {
        &self.profiles
    }

    pub fn network(&self) -> Option<NetworkFingerprint> {
        self.network
    }

    /// The profile the current network belongs to.
    pub fn current_profile(&self) -> Option<&NetworkProfile> {
        let fp = self.network?;
        self.profiles.iter().find(|p| p.fingerprints.contains(&fp))
    }

    fn set(
        &mut self,
        state: DeviceState,
        reason: StateReason,
    ) -> Option<(DeviceState, StateReason)> {
        if self.state == state {
            return None;
        }
        self.state = state;
        self.reason = reason;
        Some((state, reason))
    }

    /// A user command; returns the new state if it changed.
    pub fn command(&mut self, cmd: StateCommand) -> Option<(DeviceState, StateReason)> {
        match (cmd, self.state) {
            (StateCommand::Pause, DeviceState::Connected) => {
                self.set(DeviceState::Paused, StateReason::USER)
            }
            (StateCommand::Resume, DeviceState::Paused) => {
                self.set(DeviceState::Connected, StateReason::USER)
            }
            (StateCommand::Leave, DeviceState::Connected | DeviceState::Paused) => {
                self.set(DeviceState::Left, StateReason::USER)
            }
            (StateCommand::Rejoin, DeviceState::Left) => {
                self.set(DeviceState::Connected, StateReason::USER)
            }
            _ => None,
        }
    }

    /// The network monitor saw `fp` (None: no default route).
    pub fn network_seen(&mut self, now: Micros, fp: Option<NetworkFingerprint>) {
        if fp == self.network {
            self.pending = None;
            return;
        }
        if self.pending.is_none_or(|(p, _)| p != fp) {
            self.pending = Some((fp, now));
        }
    }

    pub fn tick(&mut self, now: Micros) -> Option<(DeviceState, StateReason)> {
        let (fp, since) = self.pending?;
        if now.saturating_sub(since) < NETWORK_DEBOUNCE {
            return None;
        }
        self.pending = None;
        self.network = fp;
        let fp = fp?;
        if self.profiles.is_empty() {
            return None;
        }
        match self.profiles.iter().find(|p| p.fingerprints.contains(&fp)) {
            Some(p)
                if p.auto_join
                    && self.state == DeviceState::Left
                    && self.reason == StateReason::NETWORK_CHANGED =>
            {
                self.set(DeviceState::Connected, StateReason::AUTO_REJOIN)
            }
            Some(_) => None,
            None if self.state != DeviceState::Left => {
                self.set(DeviceState::Left, StateReason::NETWORK_CHANGED)
            }
            None => None,
        }
    }

    /// Remembers the current network under `name` (adds it to an existing profile).
    pub fn save_profile(
        &mut self,
        name: &str,
        auto_join: bool,
    ) -> Result<&NetworkProfile, &'static str> {
        let fp = self.network.ok_or("no network to remember yet")?;
        let i = match self.profiles.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                self.profiles.push(NetworkProfile {
                    name: name.to_owned(),
                    fingerprints: Vec::new(),
                    auto_join,
                });
                self.profiles.len() - 1
            }
        };
        for (j, p) in self.profiles.iter_mut().enumerate() {
            if j != i {
                p.fingerprints.retain(|f| *f != fp);
            }
        }
        let p = &mut self.profiles[i];
        p.auto_join = auto_join;
        if !p.fingerprints.contains(&fp) {
            p.fingerprints.push(fp);
        }
        Ok(&self.profiles[i])
    }

    pub fn forget_profile(&mut self, name: &str) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|p| p.name != name);
        self.profiles.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: NetworkFingerprint = [1; 32];
    const CAFE: NetworkFingerprint = [2; 32];

    fn at(ms: u64) -> Micros {
        Micros(ms * 1_000)
    }

    #[test]
    fn commands_follow_the_state_graph() {
        let mut m = DeviceStateMachine::default();
        assert_eq!(m.command(StateCommand::Resume), None);
        assert_eq!(
            m.command(StateCommand::Pause),
            Some((DeviceState::Paused, StateReason::USER))
        );
        assert_eq!(m.command(StateCommand::Rejoin), None);
        assert_eq!(
            m.command(StateCommand::Leave).map(|s| s.0),
            Some(DeviceState::Left)
        );
        assert_eq!(
            m.command(StateCommand::Pause),
            None,
            "a device that left cannot pause"
        );
        assert_eq!(
            m.command(StateCommand::Rejoin).map(|s| s.0),
            Some(DeviceState::Connected)
        );
    }

    #[test]
    fn networks_leave_and_come_back_after_the_debounce() {
        let mut m = DeviceStateMachine::default();
        m.network_seen(at(0), Some(HOME));
        assert_eq!(m.tick(at(2_000)), None);
        assert_eq!(m.tick(at(3_100)), None, "no profiles: nothing automatic");
        assert_eq!(m.network(), Some(HOME));
        m.save_profile("Дом", true).unwrap();

        m.network_seen(at(10_000), Some(CAFE));
        m.network_seen(at(11_000), Some(HOME));
        m.network_seen(at(11_500), Some(CAFE));
        assert_eq!(m.tick(at(14_000)), None, "a flap restarts the debounce");
        assert_eq!(
            m.tick(at(14_600)),
            Some((DeviceState::Left, StateReason::NETWORK_CHANGED))
        );

        m.network_seen(at(20_000), Some(HOME));
        assert_eq!(
            m.tick(at(23_100)),
            Some((DeviceState::Connected, StateReason::AUTO_REJOIN))
        );
        assert_eq!(m.current_profile().map(|p| p.name.as_str()), Some("Дом"));
    }

    #[test]
    fn a_manual_leave_is_not_undone_by_the_network() {
        let mut m = DeviceStateMachine::default();
        m.network_seen(at(0), Some(HOME));
        m.tick(at(4_000));
        m.save_profile("home", true).unwrap();
        m.command(StateCommand::Leave);
        m.network_seen(at(5_000), Some(CAFE));
        m.tick(at(9_000));
        m.network_seen(at(10_000), Some(HOME));
        assert_eq!(m.tick(at(14_000)), None, "the user left on purpose");
        assert_eq!(m.state().0, DeviceState::Left);
    }

    #[test]
    fn profiles_collect_fingerprints() {
        let mut m = DeviceStateMachine::default();
        assert!(m.save_profile("x", false).is_err());
        m.network_seen(at(0), Some(HOME));
        m.tick(at(4_000));
        m.save_profile("Университет", false).unwrap();
        m.network_seen(at(5_000), Some(CAFE));
        m.tick(at(9_000));
        m.save_profile("Университет", true).unwrap();
        assert_eq!(
            m.profiles()[0].fingerprints,
            vec![HOME, CAFE],
            "mesh networks change the gateway MAC"
        );
        assert!(m.profiles()[0].auto_join);
        m.save_profile("Кафе", false).unwrap();
        assert_eq!(
            m.profiles()[0].fingerprints,
            vec![HOME],
            "a network belongs to one profile"
        );
        assert!(m.forget_profile("Кафе"));
        assert!(!m.forget_profile("Кафе"));
        m.network_seen(at(10_000), None);
        assert_eq!(m.tick(at(14_000)), None, "offline does not leave");
    }
}
