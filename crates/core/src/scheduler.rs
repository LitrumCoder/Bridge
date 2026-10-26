//! Maps channel classes to connections and stream priorities (ARCHITECTURE §3.1).
//! Features never pick streams themselves; future transports (BLE, Wi-Fi Direct)
//! plug in here by changing the plan, not the features.

use bridge_protocol::ChannelClass;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConnectionKind {
    /// Control, input, MIDI, clipboard: default congestion control.
    Interactive,
    /// File data only, with a delay-based congestion controller (ADR-0002).
    Bulk,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelPlan {
    pub connection: ConnectionKind,
    /// Stream priority within the connection; higher is sent first.
    pub priority: i32,
}

#[derive(Clone, Debug)]
pub struct ChannelScheduler {
    plans: [ChannelPlan; ChannelClass::ALL.len()],
}

impl Default for ChannelScheduler {
    fn default() -> Self {
        let plan = |class| match class {
            ChannelClass::Control => (ConnectionKind::Interactive, 100),
            ChannelClass::InputMotion => (ConnectionKind::Interactive, 95),
            ChannelClass::InputEvents => (ConnectionKind::Interactive, 90),
            ChannelClass::Midi => (ConnectionKind::Interactive, 80),
            ChannelClass::Clipboard => (ConnectionKind::Interactive, 50),
            ChannelClass::Bulk => (ConnectionKind::Bulk, 0),
        };
        Self {
            plans: ChannelClass::ALL.map(|c| {
                let (connection, priority) = plan(c);
                ChannelPlan {
                    connection,
                    priority,
                }
            }),
        }
    }
}

impl ChannelScheduler {
    pub fn plan(&self, class: ChannelClass) -> ChannelPlan {
        self.plans[index(class)]
    }

    pub fn set_plan(&mut self, class: ChannelClass, plan: ChannelPlan) {
        self.plans[index(class)] = plan;
    }
}

fn index(class: ChannelClass) -> usize {
    ChannelClass::ALL
        .iter()
        .position(|c| *c == class)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_sensitive_channels_outrank_bulk_and_clipboard() {
        let s = ChannelScheduler::default();
        let p = |c| s.plan(c).priority;
        assert!(p(ChannelClass::Control) > p(ChannelClass::InputEvents));
        assert!(p(ChannelClass::InputEvents) > p(ChannelClass::Midi));
        assert!(p(ChannelClass::Midi) > p(ChannelClass::Clipboard));
        assert!(p(ChannelClass::Clipboard) > p(ChannelClass::Bulk));
        assert_eq!(s.plan(ChannelClass::Bulk).connection, ConnectionKind::Bulk);
        for c in ChannelClass::ALL
            .into_iter()
            .filter(|c| *c != ChannelClass::Bulk)
        {
            assert_eq!(s.plan(c).connection, ConnectionKind::Interactive);
        }
    }

    #[test]
    fn plans_can_be_overridden() {
        let mut s = ChannelScheduler::default();
        let plan = ChannelPlan {
            connection: ConnectionKind::Bulk,
            priority: -5,
        };
        s.set_plan(ChannelClass::Clipboard, plan);
        assert_eq!(s.plan(ChannelClass::Clipboard), plan);
        assert_eq!(s.plan(ChannelClass::Midi).priority, 80);
    }
}
