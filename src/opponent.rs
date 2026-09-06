//! Recent enemy attack pressure inferred only from public events.

use std::collections::BTreeMap;

use arena_core::event::{Event, EventKind};
use arena_core::observe::Observation;
use arena_core::types::{Faction, PortalId};

/// Recent attack window and pressure bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpponentParams {
    /// How recent (rounds, inclusive) an enemy attack counts as pressure
    /// on a portal.
    pub threat_window_rounds: u32,
    /// Saturation cap of per-portal pressure (bounds the estimate; more
    /// attacks inside the window still read as this cap).
    pub pressure_cap_per_portal: u32,
}

impl Default for OpponentParams {
    fn default() -> Self {
        OpponentParams {
            threat_window_rounds: 8,
            pressure_cap_per_portal: 5,
        }
    }
}

/// The model state over one faction's public history. Constructed per
/// policy instance (one enemy faction per match side); `update` consumes
/// only events the observation has not shown yet (by `seq`), so repeated
/// observations are idempotent and incremental.
#[derive(Debug)]
pub struct OpponentModel {
    params: OpponentParams,
    enemy: Faction,
    /// Highest event `seq` already consumed.
    last_seq_seen: u64,
    /// Raw `(round, seq)` of every retained enemy attack, per portal.
    attacks: BTreeMap<PortalId, Vec<(u32, u64)>>,
}

impl OpponentModel {
    pub fn new(params: OpponentParams, enemy: Faction) -> Self {
        OpponentModel {
            params,
            enemy,
            last_seq_seen: 0,
            attacks: BTreeMap::new(),
        }
    }

    /// Consumes the NEW public events of `obs` (by `seq`) and prunes raw
    /// entries outside the attack window of `obs.round` (memory is bounded by
    /// the window sizes). A pure function of the event history prefix:
    /// deterministic and idempotent for repeated observations.
    pub fn update(&mut self, obs: &Observation) {
        let keep_from = obs.round.saturating_sub(self.params.threat_window_rounds);
        let mut fresh: Vec<&Event> = obs
            .events
            .iter()
            .filter(|event| event.seq >= self.last_seq_seen)
            .collect();
        fresh.sort_by_key(|event| event.seq);
        for event in fresh {
            self.last_seq_seen = self.last_seq_seen.max(event.seq + 1);
            match &event.kind {
                EventKind::PortalCaptured { portal, .. }
                | EventKind::PortalNeutralized { portal, .. } => {
                    self.attacks.remove(portal);
                }
                EventKind::PortalAttacked {
                    faction, portal, ..
                } if *faction == self.enemy => {
                    self.attacks
                        .entry(*portal)
                        .or_default()
                        .push((event.round, event.seq));
                }
                _ => {}
            }
        }
        let prune = |map: &mut BTreeMap<PortalId, Vec<(u32, u64)>>| {
            map.retain(|_, rounds| {
                rounds.retain(|&(round, _)| round >= keep_from);
                !rounds.is_empty()
            });
        };
        prune(&mut self.attacks);
    }

    /// Enemy attack pressure on `portal` (count of in-window attacks,
    /// capped): the threat estimate used to enable defense candidates.
    pub fn pressure(&self, obs: &Observation, portal: PortalId) -> u32 {
        self.attacks
            .get(&portal)
            .map(|rounds| {
                rounds
                    .iter()
                    .filter(|&&(round, _)| {
                        round < obs.round && obs.round - round <= self.params.threat_window_rounds
                    })
                    .count() as u32
            })
            .unwrap_or(0)
            .min(self.params.pressure_cap_per_portal)
    }

    /// Whether any portal is currently under enemy pressure.
    pub fn any_threat(&self, obs: &Observation) -> bool {
        self.attacks
            .keys()
            .any(|portal| self.pressure(obs, *portal) > 0)
    }
}
