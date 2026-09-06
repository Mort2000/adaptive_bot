//! Integer estimates for field income, attack denial, and extra income from rescue.
//! These are bounded heuristics over legal observations, not optimal-play guarantees.

use std::collections::{BTreeMap, BTreeSet};

use arena_core::config::GameConfig;
use arena_core::event::EventKind;
use arena_core::geometry::{area2, ceil_integer_sqrt, dist2, point_on_segment, segments_intersect};
use arena_core::observe::{AgentPrivate, Observation, PortalPublic};
use arena_core::types::{AgentId, Coord, Faction, LinkId, Owner, PortalId};

use crate::common::owners;

/// Integer heuristic weights and search bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValueParams {
    /// Income horizon cap (rounds): no candidate is credited with more
    /// future income than this, even early in a long match.
    pub horizon_cap_rounds: u32,
    /// How recent (rounds, inclusive) an enemy attack must be to count as
    /// pressure on a portal.
    pub threat_window_rounds: u32,
    /// Income-rounds discounted per candidate vertex under recent enemy
    /// attack (loss risk).
    pub risk_rounds_per_threatened_vertex: u32,
    /// An own anchor is defense-urgent below this share (permille) of
    /// `PORTAL_MAX_HP` (or under attack inside the threat window).
    pub defend_hp_permille: u32,
    /// Share (permille) of the best new own field through a portal
    /// credited as capture gain.
    pub capture_gain_share_permille: u32,
    /// Cost of one unit of Supply spent on RECHARGE, in permille of one
    /// round of opportunity income.
    pub supply_cost_permille: u32,
    /// Compute bound: how many ranked field candidates one decision keeps.
    pub max_field_candidates: usize,
}

impl Default for ValueParams {
    fn default() -> Self {
        ValueParams {
            horizon_cap_rounds: 100,
            threat_window_rounds: 8,
            risk_rounds_per_threatened_vertex: 10,
            defend_hp_permille: 500,
            capture_gain_share_permille: 500,
            supply_cost_permille: 500,
            max_field_candidates: 24,
        }
    }
}

/// The shared conversion terms every opportunity-cost deduction goes
/// through: how many rounds of income the model still credits, and how
/// much score2 one round of an agent's time is worth against its best
/// forgone alternative.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Economy {
    /// Remaining income horizon (rounds) the model credits.
    pub horizon_rounds: u32,
    /// Score2 forgone per round of agent time (the best pending build
    /// option's per-round income).
    pub opportunity_per_round: u64,
}

impl Economy {
    /// Derives the economy at `round`: the horizon is the remaining match
    /// length capped by `params.horizon_cap_rounds` (never zero before the
    /// final round settles); the opportunity rate is the caller's best
    /// pending own field area2 (score2 per round).
    pub fn for_round(
        cfg: &GameConfig,
        round: u32,
        params: &ValueParams,
        best_pending_own_area2: u64,
    ) -> Economy {
        Economy {
            horizon_rounds: horizon_rounds(cfg, round, params),
            opportunity_per_round: best_pending_own_area2,
        }
    }
}

/// A viable own-faction field candidate: three distinct portals whose
/// three link pairs are all buildable this round (the frozen
/// `search_triangle` edge definition) with the triangle's exact integer
/// `area2`. Vertices are canonical ascending.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FieldCandidate {
    pub vertices: [PortalId; 3],
    pub area2: u64,
}

// ---------------------------------------------------------------------
// Small shared helpers (all integer, all saturating)
// ---------------------------------------------------------------------

/// Clamps an i128 intermediate into the i64 score domain.
fn sat_i64(value: i128) -> i64 {
    value.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Whole rounds an agent needs to cover `from -> to` at one round's
/// MOVE_BUDGET (ceil of the exact integer distance). Public: the builder
/// core and policy overlays reuse the same travel estimate.
pub fn travel_rounds_between(from: Coord, to: Coord, budget_cm: u32) -> u32 {
    if budget_cm == 0 {
        return u32::MAX; // GameConfig::validate rejects this; stay total
    }
    let distance = ceil_integer_sqrt(dist2(from, to).max(0) as u64);
    distance.div_ceil(budget_cm as u64).min(u32::MAX as u64) as u32
}

/// The position view of `portal` in `obs` (portals are dense by map
/// construction; `None` only for a caller-side id bug).
fn view<'a>(obs: &'a Observation, portal: PortalId) -> Option<&'a PortalPublic> {
    obs.portals.iter().find(|p| p.id == portal)
}

/// Minimum whole rounds any own agent needs to reach `portal` (u32::MAX
/// when the faction has no agents or the portal is unknown — a candidate
/// no agent can reach scores accordingly).
pub fn travel_rounds_to(obs: &Observation, cfg: &GameConfig, portal: PortalId) -> u32 {
    let Some(target) = view(obs, portal) else {
        return u32::MAX;
    };
    obs.own_agents
        .iter()
        .map(|agent| travel_rounds_between(agent.position, target.position, cfg.move_budget_cm))
        .min()
        .unwrap_or(u32::MAX)
}

/// The binding travel estimate of a candidate: the rounds until EVERY
/// station can be occupied — the max over vertices of the per-vertex
/// nearest-agent travel.
pub fn travel_rounds_to_triangle(
    obs: &Observation,
    cfg: &GameConfig,
    vertices: &[PortalId; 3],
) -> u32 {
    vertices
        .iter()
        .map(|&v| travel_rounds_to(obs, cfg, v))
        .max()
        .unwrap_or(u32::MAX)
}

/// Key-acquisition cost (rounds): the field plan consumes one Key of each
/// vertex as a link target; every vertex no own agent currently holds a
/// Key of costs one HACK cooldown period (the conservative yield-1
/// estimate over the public inventory only).
pub fn key_rounds(
    obs: &Observation,
    _faction: Faction,
    cfg: &GameConfig,
    candidate: &FieldCandidate,
) -> u32 {
    let mut rounds = 0u32;
    for &vertex in &candidate.vertices {
        let held = obs
            .own_agents
            .iter()
            .any(|agent| agent.keys.get(&vertex).copied().unwrap_or(0) >= 1);
        if !held {
            rounds = rounds.saturating_add(cfg.hack_cooldown_rounds);
        }
    }
    rounds
}

/// Link-ordering cost (rounds): every link pair with a vertex not yet
/// owned by `faction` has to wait for that Capture before its Link can be
/// emitted (a link requires both portals owned).
pub fn link_order_rounds(obs: &Observation, faction: Faction, candidate: &FieldCandidate) -> u32 {
    let (own, _) = owners(faction);
    let [a, b, c] = candidate.vertices;
    let mut waiting = 0u32;
    for (x, y) in [(a, b), (a, c), (b, c)] {
        let own_both = [x, y]
            .iter()
            .all(|&v| view(obs, v).is_some_and(|p| p.owner == own));
        if !own_both {
            waiting += 1;
        }
    }
    waiting
}

/// One pass over the public history: for every portal, the round of its
/// most recent ownership change (CAPTURE or NEUTRALIZATION cascade) and
/// the `(count, first round)` of CURRENT-LIFECYCLE enemy attacks inside
/// the threat window. Lifecycle segmentation: an attack older than the
/// portal's last ownership change belongs to a PREVIOUS lifecycle —
/// recapture resets HP to `PORTAL_MAX_HP`, so those hits no longer
/// threaten the current anchor (the 2026-09-05 action audit: a recaptured
/// portal kept scoring positive defense value — and drawing full-HP
/// zero-effect Recharges — off its pre-recapture attack events).
struct ThreatIndex {
    last_change: BTreeMap<PortalId, u32>,
    /// portal -> (attack count in window and lifecycle, first such round).
    sieges: BTreeMap<PortalId, (u32, u32)>,
}

impl ThreatIndex {
    fn build(obs: &Observation, enemy: Faction, params: &ValueParams) -> ThreatIndex {
        let mut index = ThreatIndex {
            last_change: BTreeMap::new(),
            sieges: BTreeMap::new(),
        };
        for event in obs.events.iter() {
            match &event.kind {
                EventKind::PortalCaptured { portal, .. }
                | EventKind::PortalNeutralized { portal, .. } => {
                    index.last_change.insert(*portal, event.round);
                    // A new lifecycle starts: earlier attacks never count.
                    index.sieges.remove(portal);
                }
                EventKind::PortalAttacked {
                    faction, portal, ..
                } if *faction == enemy
                    && event.round < obs.round
                    && obs.round - event.round <= params.threat_window_rounds
                    && event.round > index.last_change.get(portal).copied().unwrap_or(0) =>
                {
                    let siege = index.sieges.entry(*portal).or_insert((0, event.round));
                    siege.0 += 1;
                    siege.1 = siege.1.min(event.round);
                }
                _ => {}
            }
        }
        index
    }

    /// Whether `portal` took a current-lifecycle enemy attack inside the
    /// threat window.
    fn under_siege(&self, portal: PortalId) -> bool {
        self.sieges.contains_key(&portal)
    }
}

/// Loss-risk discount (rounds): every candidate vertex under recent enemy
/// attack discounts `risk_rounds_per_threatened_vertex` rounds of income.
pub fn risk_rounds(
    obs: &Observation,
    faction: Faction,
    params: &ValueParams,
    candidate: &FieldCandidate,
) -> u32 {
    let index = ThreatIndex::build(obs, faction.opposite(), params);
    candidate
        .vertices
        .iter()
        .filter(|&&vertex| index.under_siege(vertex))
        .count() as u32
        * params.risk_rounds_per_threatened_vertex
}

/// The income horizon at `round`: the remaining match length (this round
/// inclusive) capped by the recorded parameter.
pub fn horizon_rounds(cfg: &GameConfig, round: u32, params: &ValueParams) -> u32 {
    let remaining = cfg.total_rounds.saturating_sub(round) + 1;
    params.horizon_cap_rounds.min(remaining)
}

/// Whole attacks needed to bring a portal from `hp` to zero at the current
/// ATTACK_DAMAGE.
pub fn attacks_needed(hp: u32, cfg: &GameConfig) -> u32 {
    hp.div_ceil(cfg.attack_damage.max(1))
}

/// The actual approach cost of one prospective attacker.
#[derive(Clone, Copy, Debug)]
pub struct AttackArrival {
    pub agent: AgentId,
    pub movement_rounds: u32,
}

impl AttackArrival {
    pub fn for_agent(
        agent: &AgentPrivate,
        target: Coord,
        cfg: &GameConfig,
        horizon: u32,
    ) -> Option<Self> {
        crate::common::movement_rounds_to_within(
            agent.position,
            target,
            cfg.portal_operation_range_cm,
            cfg.move_budget_cm,
            horizon,
        )
        .map(|movement_rounds| Self {
            agent: agent.id,
            movement_rounds,
        })
    }

    pub fn first_attack_delay(self) -> u32 {
        self.movement_rounds.saturating_sub(1)
    }
}

/// The same whole-team estimate is used before and after allocation.
/// None means the team cannot finish in time or contains a late member
/// who contributes no necessary damage. Non-positive net value cannot
/// justify spending these agents instead of their building opportunity.
pub fn strike_value(
    economy: &Economy,
    income_per_round: u128,
    needed: u32,
    team: &[AttackArrival],
) -> Option<i128> {
    let finish = (0..economy.horizon_rounds).find(|&round| {
        team.iter()
            .map(|agent| {
                round
                    .saturating_add(1)
                    .saturating_sub(agent.first_attack_delay()) as u64
            })
            .sum::<u64>()
            >= needed as u64
    })?;
    if team.iter().any(|agent| agent.first_attack_delay() > finish) {
        return None;
    }
    let travel: u128 = team.iter().map(|agent| agent.movement_rounds as u128).sum();
    Some(
        income_per_round as i128 * (economy.horizon_rounds - finish) as i128
            - (travel + needed as u128) as i128 * economy.opportunity_per_round as i128,
    )
}

// ---------------------------------------------------------------------
// Field candidate generation
// ---------------------------------------------------------------------

/// Whether the canonical pair `(a, b)` is a buildable link for the faction
/// that may build on `buildable`-or-neutral vertices — the frozen
/// `search_triangle` edge definition: within the closed
/// `MAX_LINK_DISTANCE`, no link on the pair, no third portal on the closed
/// segment, and no crossing live link (shared endpoints exempt).
fn edge_viable(
    portals: &[PortalPublic],
    links: &[arena_core::observe::LinkPublic],
    link_ids: &BTreeSet<LinkId>,
    max_link_distance_cm: u32,
    a: PortalId,
    b: PortalId,
) -> bool {
    let (Some(pa), Some(pb)) = (
        portals.iter().find(|p| p.id == a),
        portals.iter().find(|p| p.id == b),
    ) else {
        return false;
    };
    if a == b {
        return false;
    }
    let max2 = (max_link_distance_cm as i64) * (max_link_distance_cm as i64);
    if dist2(pa.position, pb.position) > max2 {
        return false;
    }
    if link_ids.contains(&LinkId::new(a, b).expect("distinct pair")) {
        return false;
    }
    if portals
        .iter()
        .any(|p| p.id != a && p.id != b && point_on_segment(p.position, pa.position, pb.position))
    {
        return false;
    }
    !links.iter().any(|link| {
        let (Some(u), Some(v)) = (
            portals.iter().find(|p| p.id == link.id.min),
            portals.iter().find(|p| p.id == link.id.max),
        ) else {
            return false;
        };
        let shared = link.id.contains(a) || link.id.contains(b);
        !shared && segments_intersect(pa.position, pb.position, u.position, v.position)
    })
}

/// Every viable candidate triangle over vertices the `buildable` faction
/// may use (own-or-neutral from its perspective), with exact `area2`.
/// Deterministic: ascending vertex triples in enumeration order.
fn candidate_triples(
    portals: &[PortalPublic],
    links: &[arena_core::observe::LinkPublic],
    buildable: Owner,
    max_link_distance_cm: u32,
) -> Vec<([PortalId; 3], u64)> {
    let link_ids: BTreeSet<LinkId> = links.iter().map(|l| l.id).collect();
    let reusable: BTreeSet<LinkId> = links
        .iter()
        .filter(|link| owners(link.faction).0 == buildable)
        .map(|link| link.id)
        .collect();
    let eligible: Vec<PortalId> = portals
        .iter()
        .filter(|p| p.owner == Owner::Neutral || p.owner == buildable)
        .map(|p| p.id)
        .collect();
    let eligible_set: BTreeSet<PortalId> = eligible.iter().copied().collect();

    // Adjacency over buildable pairs (each pair enumerated once, a < b).
    let mut adjacency: std::collections::BTreeMap<PortalId, BTreeSet<PortalId>> =
        std::collections::BTreeMap::new();
    for (index, &a) in eligible.iter().enumerate() {
        for &b in eligible.iter().skip(index + 1) {
            if reusable.contains(&LinkId::new(a, b).unwrap())
                || edge_viable(portals, links, &link_ids, max_link_distance_cm, a, b)
            {
                adjacency.entry(a).or_default().insert(b);
                adjacency.entry(b).or_default().insert(a);
            }
        }
    }

    let position = |id: PortalId| -> Coord {
        portals
            .iter()
            .find(|p| p.id == id)
            .expect("dense portal ids")
            .position
    };
    let mut triples = Vec::new();
    for (&a, neighbors) in adjacency.iter() {
        for &b in neighbors
            .iter()
            .filter(|b| eligible_set.contains(b) && **b > a)
        {
            let common: Vec<PortalId> = neighbors
                .iter()
                .filter(|c| **c > b && adjacency.get(c).is_some_and(|n| n.contains(&b)))
                .copied()
                .collect();
            for c in common {
                if [
                    LinkId::new(a, b).unwrap(),
                    LinkId::new(a, c).unwrap(),
                    LinkId::new(b, c).unwrap(),
                ]
                .iter()
                .all(|link| reusable.contains(link))
                {
                    continue;
                }
                let area = area2(position(a), position(b), position(c));
                if area > 0 {
                    triples.push(([a, b, c], area));
                }
            }
        }
    }
    triples
}

/// This round's viable own-faction field candidates for `faction`, ranked
/// by existing own-edge count, then `area2` descending and triple, and
/// truncated to `params.max_field_candidates` (the compute bound of plan
/// section 15; value ranking happens in [`rank_field_candidates`]).
pub fn field_candidates(
    obs: &Observation,
    faction: Faction,
    cfg: &GameConfig,
    params: &ValueParams,
) -> Vec<FieldCandidate> {
    let (own, _) = owners(faction);
    let mut candidates: Vec<FieldCandidate> =
        candidate_triples(&obs.portals, &obs.links, own, cfg.max_link_distance_cm)
            .into_iter()
            .map(|(vertices, area2)| FieldCandidate { vertices, area2 })
            .collect();
    let own_links: BTreeSet<_> = obs
        .links
        .iter()
        .filter(|link| link.faction == faction)
        .map(|link| link.id)
        .collect();
    candidates.sort_by_cached_key(|candidate| {
        let [a, b, c] = candidate.vertices;
        let completed = [(a, b), (a, c), (b, c)]
            .into_iter()
            .filter(|&(s, t)| own_links.contains(&LinkId::new(s, t).unwrap()))
            .count();
        (
            std::cmp::Reverse(completed),
            std::cmp::Reverse(candidate.area2),
            candidate.vertices,
        )
    });
    candidates.truncate(params.max_field_candidates);
    candidates
}

/// Full value ranking of [`field_candidates`] for `faction`: every
/// candidate scored by [`field_value`], sorted value-descending with
/// ascending-triple tie-breaks. A pure function of the observation —
/// equal observations rank identically.
pub fn rank_field_candidates(
    obs: &Observation,
    faction: Faction,
    cfg: &GameConfig,
    params: &ValueParams,
) -> Vec<(i64, FieldCandidate)> {
    let mut ranked: Vec<(i64, FieldCandidate)> = field_candidates(obs, faction, cfg, params)
        .into_iter()
        .map(|candidate| {
            (
                field_value(obs, faction, cfg, params, &candidate),
                candidate,
            )
        })
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.vertices.cmp(&b.1.vertices)));
    ranked
}

// ---------------------------------------------------------------------
// Task value estimates
// ---------------------------------------------------------------------

/// FIELD value: the candidate's expected area-time income — `area2` times
/// the horizon net of every setup deduction, each deduction expressed as
/// rounds of the candidate's own income:
///
/// - travel ([`travel_rounds_to_triangle`]): rounds until every station
///   can be occupied;
/// - key acquisition ([`key_rounds`]): HACK cooldowns for missing target
///   keys;
/// - link ordering ([`link_order_rounds`]): link pairs waiting on
///   captures;
/// - loss risk ([`risk_rounds`]): income-rounds discounted per vertex
///   under recent enemy attack (public event history).
///
/// May be negative: a candidate whose setup outlasts its horizon destroys
/// income, and ranks below every candidate that does not.
pub fn field_value(
    obs: &Observation,
    faction: Faction,
    cfg: &GameConfig,
    params: &ValueParams,
    candidate: &FieldCandidate,
) -> i64 {
    let horizon = horizon_rounds(cfg, obs.round, params) as i64;
    let [a, b, c] = candidate.vertices;
    let pending: Vec<_> = [(a, b), (a, c), (b, c)]
        .into_iter()
        .filter(|&(s, t)| {
            !obs.links
                .iter()
                .any(|link| link.faction == faction && link.id == LinkId::new(s, t).unwrap())
        })
        .collect();
    if let [(source, target)] = pending.as_slice() {
        let delay = obs
            .own_agents
            .iter()
            .flat_map(|agent| {
                [(*source, *target), (*target, *source)]
                    .into_iter()
                    .filter_map(move |(s, t)| {
                        crate::common::link_completion_delay(
                            obs,
                            owners(faction).0,
                            cfg,
                            agent,
                            s,
                            t,
                        )
                    })
            })
            .min();
        if let Some(delay) = delay {
            let saved_rounds = horizon
                .saturating_sub(delay as i64)
                .saturating_sub(risk_rounds(obs, faction, params, candidate) as i64);
            return sat_i64(candidate.area2 as i128 * saved_rounds as i128);
        }
    }
    let setup = [
        travel_rounds_to_triangle(obs, cfg, &candidate.vertices),
        key_rounds(obs, faction, cfg, candidate),
        link_order_rounds(obs, faction, candidate),
        risk_rounds(obs, faction, params, candidate),
    ]
    .iter()
    .fold(0i64, |sum, rounds| sum.saturating_add(*rounds as i64));
    sat_i64(candidate.area2 as i128 * horizon.saturating_sub(setup) as i128)
}

/// The enemy portal's contribution to enemy income that dies with it:
/// every enemy field anchored on the portal, times the horizon
/// (score2-equivalents; the denial term of [`attack_value`]).
fn preventable_income(
    obs: &Observation,
    enemy: Faction,
    horizon_rounds: u32,
    portal: PortalId,
) -> u128 {
    obs.fields
        .iter()
        .filter(|field| field.faction == enemy && field.id.contains(portal))
        .map(|field| field.area2 as u128 * horizon_rounds as u128)
        .sum()
}

/// The best own candidate triangle `faction` could build through `portal`
/// (its capture gain potential), over own-or-neutral other vertices with
/// viable pairs. Zero when the portal unlocks nothing.
pub fn best_own_triangle_through(
    obs: &Observation,
    faction: Faction,
    cfg: &GameConfig,
    portal: PortalId,
) -> u64 {
    let (own, _) = owners(faction);
    let Some(target) = view(obs, portal) else {
        return 0;
    };
    let _ = target;
    let link_ids: BTreeSet<LinkId> = obs.links.iter().map(|l| l.id).collect();
    let others: Vec<PortalId> = obs
        .portals
        .iter()
        .filter(|p| p.id != portal && (p.owner == Owner::Neutral || p.owner == own))
        .map(|p| p.id)
        .collect();
    let position = |id: PortalId| -> Coord {
        obs.portals
            .iter()
            .find(|p| p.id == id)
            .expect("dense portal ids")
            .position
    };
    let mut best = 0u64;
    for (index, &x) in others.iter().enumerate() {
        for &y in others.iter().skip(index + 1) {
            let (px, py, pp) = (position(x), position(y), position(portal));
            // Cheap necessary conditions first; full edge viability after.
            if dist2(pp, px) > (cfg.max_link_distance_cm as i64).pow(2)
                || dist2(pp, py) > (cfg.max_link_distance_cm as i64).pow(2)
                || dist2(px, py) > (cfg.max_link_distance_cm as i64).pow(2)
            {
                continue;
            }
            let viable = edge_viable(
                &obs.portals,
                &obs.links,
                &link_ids,
                cfg.max_link_distance_cm,
                portal,
                x,
            ) && edge_viable(
                &obs.portals,
                &obs.links,
                &link_ids,
                cfg.max_link_distance_cm,
                portal,
                y,
            ) && edge_viable(
                &obs.portals,
                &obs.links,
                &link_ids,
                cfg.max_link_distance_cm,
                x,
                y,
            );
            if viable {
                best = best.max(area2(pp, px, py));
            }
        }
    }
    best
}

/// Income denied or unlocked for each round after neutralization. The
/// overlay uses the actual strike's completion time and total travel cost.
pub fn attack_income_per_round(
    obs: &Observation,
    faction: Faction,
    cfg: &GameConfig,
    params: &ValueParams,
    portal: PortalId,
) -> u128 {
    preventable_income(obs, faction.opposite(), 1, portal)
        + best_own_triangle_through(obs, faction, cfg, portal) as u128
            * params.capture_gain_share_permille as u128
            / 1000
}

/// Scores the actual prospective defender, including remote Key access,
/// Supply and its own arrival deadline. The event index is shared across
/// every candidate in one observation; the allocator never substitutes
/// another agent's shorter journey for the executor's journey.
pub struct DefenseModel<'a> {
    obs: &'a Observation,
    faction: Faction,
    cfg: &'a GameConfig,
    params: &'a ValueParams,
    economy: &'a Economy,
    threats: ThreatIndex,
}

impl<'a> DefenseModel<'a> {
    pub fn new(
        obs: &'a Observation,
        faction: Faction,
        cfg: &'a GameConfig,
        params: &'a ValueParams,
        economy: &'a Economy,
    ) -> Self {
        Self {
            obs,
            faction,
            cfg,
            params,
            economy,
            threats: ThreatIndex::build(obs, faction.opposite(), params),
        }
    }

    pub fn value(&self, agent: &AgentPrivate, portal: PortalId) -> i64 {
        let Some(target) = view(self.obs, portal) else {
            return i64::MIN;
        };
        if target.owner != owners(self.faction).0
            || target.hp >= self.cfg.portal_max_hp
            || agent.supply < self.cfg.recharge_cost_supply
        {
            return 0;
        }
        let preserved_area: u128 = self
            .obs
            .fields
            .iter()
            .filter(|f| f.faction == self.faction && f.id.contains(portal))
            .map(|f| f.area2 as u128)
            .sum();
        if preserved_area == 0 {
            return 0;
        }
        let hp_low = target.hp as u64 * 1000
            < self.cfg.portal_max_hp as u64 * self.params.defend_hp_permille as u64;
        if !hp_low && !self.threats.under_siege(portal) {
            return 0;
        }
        let travel = if agent.keys.get(&portal).copied().unwrap_or(0) > 0 {
            0
        } else {
            let Some(moves) = crate::common::movement_rounds_to_within(
                agent.position,
                target.position,
                self.cfg.portal_operation_range_cm,
                self.cfg.move_budget_cm,
                self.cfg.total_rounds.saturating_sub(self.obs.round) + 1,
            ) else {
                return 0;
            };
            moves
        };
        // Move is auxiliary: reaching operation range permits Recharge in
        // the same stream, exactly as for Attack. Only earlier moves delay it.
        let delay = travel.saturating_sub(1);
        let arrival = delay.saturating_add(1);
        // Credit only the extra lifetime bought by THIS Recharge. Under
        // observed pressure use its settled damage rate; a damaged, quiet
        // anchor uses one attack per round as a conservative reserve value.
        let (hits, elapsed) = self
            .threats
            .sieges
            .get(&portal)
            .map_or((1, 1), |&(hits, first)| {
                (
                    hits,
                    self.obs
                        .round
                        .saturating_sub(first)
                        .min(self.params.threat_window_rounds)
                        .max(1),
                )
            });
        let damage = (hits as u128 * self.cfg.attack_damage.max(1) as u128)
            .div_ceil(elapsed as u128)
            .max(1);
        let arrival_hp = (target.hp as u128).saturating_sub(damage * delay as u128);
        if arrival_hp == 0 {
            return 0;
        }
        // Arrival may be the nominal collapse round: a defender acting
        // first can still rescue it. Evaluate initiative at that round,
        // after charging the damage received during earlier movement.
        let acts_first =
            (self.obs.round.saturating_add(delay) % 2 == 1) == (self.faction == Faction::Enl);
        if !acts_first
            && self.threats.under_siege(portal)
            && arrival_hp <= self.cfg.attack_damage as u128
        {
            return 0;
        }
        let remaining = self.economy.horizon_rounds.saturating_sub(delay) as u128;
        let unassisted = arrival_hp.saturating_sub(1) / damage;
        let heal = (self.cfg.recharge_amount as u128)
            .min((self.cfg.portal_max_hp as u128).saturating_sub(arrival_hp));
        let assisted = (arrival_hp + heal).saturating_sub(1) / damage;
        let preserved = preserved_area
            * assisted
                .min(remaining)
                .saturating_sub(unassisted.min(remaining));
        let action_cost = arrival as u128 * self.economy.opportunity_per_round as u128;
        let supply_cost = self.cfg.recharge_cost_supply as u128
            * self.params.supply_cost_permille as u128
            * self.economy.opportunity_per_round as u128
            / 1000;
        sat_i64((preserved as i128 - action_cost as i128 - supply_cost as i128) / arrival as i128)
    }

    /// Net value of one explicitly coordinated same-round rescue. This is
    /// one batch of Recharge actions, not a promise of free future upkeep.
    /// Remote Keys or one Move must make every member available now. Heals
    /// are interleaved with estimated pressure using faction initiative
    /// and an attack-first enemy queue (the actual future queue is unknown);
    /// a dead anchor cannot be revived. Capacity is also bounded by the
    /// observed missing HP: predicted hits are not guaranteed to occur
    /// between allies (or at all), so they cannot justify a redundant heal.
    pub fn coordinated_value(&self, agents: &[AgentId], portal: PortalId) -> i64 {
        if self.cfg.attack_damage == 0 || self.cfg.recharge_amount == 0 {
            return 0;
        }
        if agents.len() == 1 {
            return self
                .obs
                .own_agents
                .iter()
                .find(|a| a.id == agents[0])
                .map_or(0, |agent| self.value(agent, portal));
        }
        let Some(target) = view(self.obs, portal) else {
            return 0;
        };
        if agents.is_empty()
            || target.owner != owners(self.faction).0
            || target.hp >= self.cfg.portal_max_hp
        {
            return 0;
        }
        let missing_hp = self.cfg.portal_max_hp.saturating_sub(target.hp) as u128;
        if (agents.len() - 1) as u128 * self.cfg.recharge_amount as u128 >= missing_hp {
            return 0;
        }
        let hp_low = target.hp as u64 * 1000
            < self.cfg.portal_max_hp as u64 * self.params.defend_hp_permille as u64;
        if !hp_low && !self.threats.under_siege(portal) {
            return 0;
        }
        if agents.iter().any(|id| {
            self.obs
                .own_agents
                .iter()
                .find(|a| a.id == *id)
                .is_none_or(|a| !self.available_now(a, portal))
        }) {
            return 0;
        }
        if agents.iter().copied().collect::<BTreeSet<_>>().len() != agents.len() {
            return 0;
        }
        let area: u128 = self
            .obs
            .fields
            .iter()
            .filter(|f| f.faction == self.faction && f.id.contains(portal))
            .map(|f| f.area2 as u128)
            .sum();
        if area == 0 {
            return 0;
        }
        let (hits, elapsed) = self
            .threats
            .sieges
            .get(&portal)
            .map_or((1, 1), |&(hits, first)| {
                (
                    hits,
                    self.obs
                        .round
                        .saturating_sub(first)
                        .min(self.params.threat_window_rounds)
                        .max(1),
                )
            });
        let damage = (hits as u128 * self.cfg.attack_damage as u128)
            .div_ceil(elapsed as u128)
            .max(1);
        let own_first = (self.obs.round % 2 == 1) == (self.faction == Faction::Enl);
        if !own_first && self.threats.under_siege(portal) && target.hp <= self.cfg.attack_damage {
            return 0;
        }
        let mut hp = target.hp as u128;
        let mut pending_damage = damage;
        let mut healed = 0;
        let mut own_turn = own_first;
        while pending_damage > 0 || healed < agents.len() {
            if own_turn && healed < agents.len() {
                let next =
                    (hp + self.cfg.recharge_amount as u128).min(self.cfg.portal_max_hp as u128);
                if next == hp {
                    return 0;
                }
                hp = next;
                healed += 1;
            } else if !own_turn && pending_damage > 0 {
                let hit = pending_damage.min(self.cfg.attack_damage as u128);
                hp = hp.saturating_sub(hit);
                pending_damage -= hit;
                if hp == 0 {
                    return 0;
                }
            }
            own_turn = !own_turn;
        }
        let horizon = self.economy.horizon_rounds as u128;
        let unassisted = (target.hp as u128).saturating_sub(1) / damage;
        let assisted = 1 + hp.saturating_sub(1) / damage;
        let gained = assisted
            .min(horizon)
            .saturating_sub(unassisted.min(horizon));
        let actions = agents.len() as u128 * self.economy.opportunity_per_round as u128;
        let supply = actions
            * self.cfg.recharge_cost_supply as u128
            * self.params.supply_cost_permille as u128
            / 1000;
        sat_i64((area * gained) as i128 - actions as i128 - supply as i128)
    }

    pub fn available_now(&self, agent: &AgentPrivate, portal: PortalId) -> bool {
        let Some(target) = view(self.obs, portal) else {
            return false;
        };
        agent.supply >= self.cfg.recharge_cost_supply
            && (agent.keys.get(&portal).copied().unwrap_or(0) > 0
                || crate::common::movement_rounds_to_within(
                    agent.position,
                    target.position,
                    self.cfg.portal_operation_range_cm,
                    self.cfg.move_budget_cm,
                    1,
                )
                .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit-level pins of the helpers the integration fixtures exercise
    // only through rankings.

    #[test]
    fn travel_rounds_use_exact_integer_ceil_sqrt() {
        // 30_000 cm at a 20_000 cm budget: ceil(sqrt(900_000_000)/20_000)
        // = ceil(30_000/20_000) = 2 rounds.
        assert_eq!(
            travel_rounds_between(Coord::new(0, 0), Coord::new(40_000, 0), 20_000),
            2
        );
        // The exact budget distance is one round (closed range).
        assert_eq!(
            travel_rounds_between(Coord::new(0, 0), Coord::new(20_000, 0), 20_000),
            1
        );
        // Diagonals use the exact integer root, not axis sums.
        assert_eq!(
            travel_rounds_between(Coord::new(0, 0), Coord::new(20_000, 20_000), 20_000),
            2 // sqrt(8)*10_000 = 28_284 -> 2 rounds
        );
        assert_eq!(
            travel_rounds_between(Coord::new(5, 7), Coord::new(5, 7), 20_000),
            0
        );
    }

    #[test]
    fn attacks_needed_is_the_ceil_of_hp_over_damage() {
        let mut cfg = GameConfig::default();
        cfg.attack_damage = 20;
        assert_eq!(attacks_needed(100, &cfg), 5);
        assert_eq!(attacks_needed(101, &cfg), 6);
        assert_eq!(attacks_needed(1, &cfg), 1);
        assert_eq!(attacks_needed(0, &cfg), 0);
        cfg.attack_damage = 0; // GameConfig::validate rejects this
        assert_eq!(attacks_needed(100, &cfg), 100, "defensive nonzero divisor");
    }

    #[test]
    fn horizon_never_reaches_zero_before_settlement() {
        let cfg = GameConfig::default();
        let mut params = ValueParams::default();
        params.horizon_cap_rounds = 1000; // above the remaining match
        assert_eq!(horizon_rounds(&cfg, 1, &params), 500);
        assert_eq!(horizon_rounds(&cfg, 500, &params), 1);
        assert_eq!(horizon_rounds(&cfg, 501, &params), 1, "saturates, never 0");
        params.horizon_cap_rounds = 3;
        assert_eq!(horizon_rounds(&cfg, 1, &params), 3);
    }
}
