//! Build fields, contest enemy anchors, and maintain threatened income.
//! Every agent (including reinforcements) uses the same task allocator.
use arena_core::config::GameConfig;
use arena_core::harness::AgentSubmission;
use arena_core::map::MapDefinition;
use arena_core::observe::Observation;
use arena_core::types::{AgentId, Coord, Faction};

use crate::attack::attack_overlay;
use crate::build::{BuilderCore, CoreParams};
use crate::common;
use crate::defense::{defense_overlay, merge_requests};
use crate::opponent::{OpponentModel, OpponentParams};

/// The Alpha reference strategy, extracted from Adaptive r2.6.
/// Owns match-local memory; receives only the legal faction observation.
pub struct Adaptive {
    core: BuilderCore,
    opponent: OpponentModel,
}

impl Adaptive {
    pub fn new(faction: Faction, seed: u64, config: &GameConfig) -> Self {
        let params = CoreParams {
            supply_reserve_recharges: 5,
            ..CoreParams::default()
        };
        Self {
            core: BuilderCore::new(faction, seed, config, params),
            opponent: OpponentModel::new(OpponentParams::default(), faction.opposite()),
        }
    }

    /// Pick the complete initial spawn set from the public map.
    pub fn choose_spawns(
        &mut self,
        map: &MapDefinition,
        roster: &[AgentId],
    ) -> Vec<(AgentId, Coord)> {
        let config = *self.core.cfg();
        self.core.choose_spawns(&config, map, roster)
    }

    /// Produce ordered per-agent command streams for one round.
    pub fn submit_round(&mut self, obs: &Observation) -> Vec<AgentSubmission> {
        self.core.refresh(obs);
        self.opponent.update(obs);
        let mut requests = if self.core.active().is_some() {
            self.core.build_requests(obs)
        } else {
            self.core.fallback_requests(obs)
        };
        let endgame = obs.round as u64 * 1000 / self.core.cfg().total_rounds.max(1) as u64 >= 850;
        if self.opponent.any_threat(obs) || endgame {
            merge_requests(&mut requests, defense_overlay(&self.core, obs, 2));
        }
        merge_requests(&mut requests, attack_overlay(&self.core, obs, 1000));
        let mut commands = self.core.allocate_and_execute(obs, &requests);
        let mut submissions: Vec<_> = obs
            .own_agents
            .iter()
            .filter_map(|agent| {
                commands
                    .remove(&agent.id)
                    .map(|list| common::submission(agent.id, list.build()))
            })
            .collect();
        // Urgent remote maintenance settles before ordinary construction.
        submissions.sort_by_key(|submission| {
            !submission
                .commands
                .iter()
                .any(|command| matches!(command, arena_core::types::Command::Recharge { .. }))
        });
        submissions
    }
}
