//! Coordinated attacks compete with construction using their denial value.
use crate::build::BuilderCore;
use crate::common;
use crate::tasks::{Task, TaskRequest};
use crate::value::{AttackArrival, attack_income_per_round, attacks_needed, strike_value};
use arena_core::observe::Observation;
use arena_core::types::AgentId;
use std::collections::BTreeMap;

/// Propose the best positive-value strike among arrival-ordered team
/// prefixes. Complete prefixes cross discrete damage thresholds even when
/// one additional attacker alone cannot advance completion. The team's
/// value is shared across requests and competes with productive building.
pub fn attack_overlay(
    core: &BuilderCore,
    obs: &Observation,
    attack_share_permille: u32,
) -> BTreeMap<AgentId, Vec<TaskRequest>> {
    let mut requests = BTreeMap::new();
    if attack_share_permille == 0 {
        return requests;
    }
    let cfg = core.cfg();
    let economy = core.economy(obs);
    let enemy_owner = common::owners(core.faction()).1;
    let share_cap =
        ((obs.own_agents.len() as u64 * attack_share_permille as u64) / 1000).max(1) as usize;
    let mut best_value = 0i128;
    for target in obs.portals.iter().filter(|p| p.owner == enemy_owner) {
        let needed = attacks_needed(target.hp, cfg).max(1);
        let income =
            attack_income_per_round(obs, core.faction(), cfg, &core.params().value, target.id);
        if income == 0 {
            continue;
        }
        let mut arrivals: Vec<_> = obs
            .own_agents
            .iter()
            .filter_map(|agent| {
                AttackArrival::for_agent(agent, target.position, cfg, economy.horizon_rounds)
            })
            .collect();
        arrivals.sort_by_key(|agent| {
            (
                agent.first_attack_delay(),
                agent.agent,
                agent.movement_rounds,
            )
        });
        let mut selected = Vec::new();
        for arrival in arrivals {
            if selected.len() >= share_cap.min(needed as usize) {
                break;
            }
            selected.push(arrival);
            let Some(value) = strike_value(&economy, income, needed, &selected) else {
                continue;
            };
            if value > best_value {
                best_value = value;
                let capacity = selected.len() as u32;
                // The whole strike's benefit is shared by its members;
                // it is not credited in full to every attacker separately.
                let priority = (value / capacity as i128).min(i64::MAX as i128) as i64;
                requests = selected
                    .iter()
                    .map(|arrival| {
                        (
                            arrival.agent,
                            vec![TaskRequest::coordinated(
                                Task::Attack { portal: target.id },
                                priority,
                                capacity,
                            )],
                        )
                    })
                    .collect();
            }
        }
    }
    requests
}
