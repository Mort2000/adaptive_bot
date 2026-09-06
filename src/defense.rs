//! Rank single-agent and coordinated rescues by extra settled field income.
use crate::build::BuilderCore;
use crate::tasks::{Task, TaskRequest};
use crate::value::DefenseModel;
use arena_core::observe::Observation;
use arena_core::types::{AgentId, PortalId};
use std::collections::BTreeMap;

pub fn defense_overlay(
    core: &BuilderCore,
    obs: &Observation,
    cap: u32,
) -> BTreeMap<AgentId, Vec<TaskRequest>> {
    let mut requests = BTreeMap::new();
    if cap == 0 {
        return requests;
    }
    let params = core.params();
    let economy = core.economy(obs);
    let model = DefenseModel::new(obs, core.faction(), core.cfg(), &params.value, &economy);
    struct Rescue {
        priority: i64,
        portal: PortalId,
        team: Vec<AgentId>,
        capacity: u32,
    }
    let mut urgent: Vec<Rescue> = obs
        .portals
        .iter()
        .filter(|p| p.owner == crate::common::owners(core.faction()).0)
        .map(|p| {
            let mut rescue = Rescue {
                priority: obs
                    .own_agents
                    .iter()
                    .map(|agent| model.value(agent, p.id))
                    .max()
                    .unwrap_or(0),
                portal: p.id,
                team: Vec::new(),
                capacity: 1,
            };
            let mut ready: Vec<_> = obs
                .own_agents
                .iter()
                .filter(|a| model.available_now(a, p.id))
                .map(|a| a.id)
                .collect();
            ready.sort();
            for size in 2..=ready.len().min(cap as usize) {
                let priority = model.coordinated_value(&ready[..size], p.id) / size as i64;
                if priority > rescue.priority {
                    rescue.priority = priority;
                    // All ready defenders have one occupied action round;
                    // let allocation substitute an equally ready teammate
                    // when another candidate wins one of the initial members.
                    rescue.team = ready.clone();
                    rescue.capacity = size as u32;
                }
            }
            rescue
        })
        .filter(|rescue| rescue.priority > 0)
        .collect();
    urgent.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.portal.cmp(&b.portal))
    });
    let mut remaining = cap as usize;
    urgent.retain(|rescue| {
        let needed = rescue.capacity as usize;
        if needed > remaining {
            return false;
        }
        remaining -= needed;
        true
    });
    if urgent.is_empty() {
        return requests;
    }
    for me in obs.own_agents.iter() {
        let mut mine = Vec::new();
        for rescue in &urgent {
            if !rescue.team.is_empty() {
                if rescue.team.contains(&me.id) {
                    mine.push(TaskRequest::coordinated(
                        Task::Recharge {
                            portal: rescue.portal,
                        },
                        rescue.priority,
                        rescue.capacity,
                    ));
                }
                continue;
            }
            let priority = model.value(me, rescue.portal);
            if priority > 0 {
                mine.push(TaskRequest::one_shot(
                    Task::Recharge {
                        portal: rescue.portal,
                    },
                    priority,
                ));
            }
        }
        requests.insert(me.id, mine);
    }
    requests
}

/// Merges overlay requests into the per-agent request map (the overlay
/// appends; the allocator arbitrates by priority).
pub fn merge_requests(
    into: &mut BTreeMap<AgentId, Vec<TaskRequest>>,
    overlay: BTreeMap<AgentId, Vec<TaskRequest>>,
) {
    for (agent, mut mine) in overlay {
        into.entry(agent).or_default().append(&mut mine);
    }
}
