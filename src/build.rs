//! Field planning, task generation, and coordinated execution.
//! Keep at most two compatible field plans; reserve tasks and geometry per round.
//! Replan completed or invalid structures, then send surplus agents to useful work.

use std::collections::BTreeMap;

use arena_core::config::GameConfig;
use arena_core::map::MapDefinition;
use arena_core::observe::Observation;
use arena_core::types::{AgentId, Coord, Faction, PortalId};

use crate::common::{self, CommandList, HackLedger, in_range, owners, step_toward};
use crate::rng::BotRng;
use crate::sample::pick_near_equal;
use crate::tasks::{Task, TaskBoard, TaskRequest, allocate, allocate_compatible};
use crate::value::{
    Economy, FieldCandidate, ValueParams, field_candidates, field_value, rank_field_candidates,
};

/// Heuristic parameters for planning, tie breaks, and replenishment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoreParams {
    /// Near-equal band (permille of the best value) the seeded top-candidate
    /// draw samples from — the tie-break that keeps mirror matches from
    /// colliding on the identical plan.
    pub near_equal_permille: u32,
    /// Rounds between re-searches while NO viable triangle exists (a failed
    /// search is not retried every round; successful replans are immediate).
    pub replan_retry_rounds: u32,
    /// Priority of an opportunistic fallback LINK (score2-equivalents): any
    /// positive value means "better than walking", while staying far below
    /// every real build value so fallback work never preempts a live plan.
    pub fallback_priority: i64,
    /// Seeded spawn jitter (cm): each roster agent spawns at its plan
    /// vertex plus a faction-keyed random offset in
    /// `[-jitter..=jitter] x [-jitter..=jitter]` (clamped in bounds).
    /// This diversifies mirror spawns even when only one triangle is viable.
    pub spawn_jitter_cm: i32,
    /// Discount for a second non-crossing field plan, so spare agents can
    /// build while the primary plan keeps its higher priority. Zero disables it.
    pub second_plan_discount_permille: u32,
    /// Independently replenish at or below this many Recharge costs.
    /// Zero disables proactive replenishment.
    pub supply_reserve_recharges: u32,
    /// The shared value model's thresholds.
    pub value: ValueParams,
}

impl Default for CoreParams {
    fn default() -> Self {
        CoreParams {
            near_equal_permille: 100,
            replan_retry_rounds: 25,
            fallback_priority: 1,
            spawn_jitter_cm: 1_000,
            second_plan_discount_permille: 750,
            supply_reserve_recharges: 0,
            value: ValueParams::default(),
        }
    }
}

/// The viability of the cached plan this round (the frozen `FieldPlanner`
/// semantics, re-stated for the value-ranked candidate).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlanStatus {
    Active,
    Complete,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDecision {
    pub agent: AgentId,
    pub grant: Option<Task>,
    /// A compatible second task sharing this stream, not another Major Action.
    pub companion: Option<Task>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundAllocation {
    pub agents: Vec<AgentDecision>,
}

pub struct BuilderCore {
    faction: Faction,
    own: arena_core::types::Owner,
    cfg: GameConfig,
    params: CoreParams,
    rng: BotRng,
    ledger: HackLedger,
    /// The chosen plan (possibly stale).
    plan: Option<FieldCandidate>,
    /// The plan validated for THIS round (`None` = fallback mode).
    active: Option<FieldCandidate>,
    next_search_round: u32,
    /// The second plan: the best value-ranked candidate whose edges
    /// cross neither the active plan's pending edges nor any live link —
    /// the task source for agents the primary triangle cannot employ.
    /// `None` in fallback mode and while the discount is 0.
    second: Option<FieldCandidate>,
    next_second_search_round: u32,
    /// Best pending own candidate area2 of the last search (the economy's
    /// opportunity rate; 0 when the map offered nothing).
    best_pending_area2: u64,
}

impl BuilderCore {
    pub fn new(faction: Faction, bot_seed: u64, cfg: &GameConfig, params: CoreParams) -> Self {
        let (own, _) = owners(faction);
        BuilderCore {
            faction,
            own,
            cfg: *cfg,
            params,
            rng: BotRng::new(bot_seed, faction),
            ledger: HackLedger::new(),
            plan: None,
            active: None,
            next_search_round: 1,
            second: None,
            next_second_search_round: 1,
            best_pending_area2: 0,
        }
    }

    /// The spawn-time candidate view of the frozen map: every portal as
    /// defined, no links, no fields, no agents — exactly the legal
    /// pre-Round-1 information.
    fn map_observation(map: &MapDefinition) -> Observation {
        use arena_core::event::Event;
        Observation {
            round: 1,
            portals: map
                .portals()
                .iter()
                .map(|p| arena_core::observe::PortalPublic {
                    id: p.id,
                    position: p.position,
                    owner: p.owner,
                    hp: p.hp,
                })
                .collect(),
            links: Vec::new(),
            fields: Vec::new(),
            events: Vec::<Event>::new(),
            scores: BTreeMap::new(),
            own_agents: Vec::new(),
        }
    }

    /// Full-map initial spawn strategy: the near-equal seeded pick over the
    /// map's field candidates, roster agent `i` on the chosen triangle's
    /// vertex `i % 3` (the plan seeds itself with the same pick); without a
    /// viable triangle, a seeded rotation over the map's portals.
    pub fn choose_spawns(
        &mut self,
        cfg: &GameConfig,
        map: &MapDefinition,
        roster: &[AgentId],
    ) -> Vec<(AgentId, Coord)> {
        let obs = Self::map_observation(map);
        let candidates = field_candidates(&obs, self.faction, cfg, &self.params.value);
        self.best_pending_area2 = candidates.first().map(|c| c.area2).unwrap_or(0);
        let picked = pick_near_equal(
            &candidates,
            |candidate| candidate.area2 as i64,
            self.params.near_equal_permille,
            &mut self.rng,
        )
        .copied();
        self.plan = picked;
        self.active = picked;
        let jitter = self.params.spawn_jitter_cm.max(0);
        let (width, height) = (cfg.map_width_cm, cfg.map_height_cm);
        let bounds = |position: Coord| -> Coord {
            Coord::new(position.x.clamp(0, width), position.y.clamp(0, height))
        };
        match picked {
            Some(candidate) => roster
                .iter()
                .enumerate()
                .map(|(index, &agent)| {
                    let portal = map
                        .portal(candidate.vertices[index % 3])
                        .expect("search only returns map portals");
                    // Seeded faction-keyed jitter: the station is the
                    // vertex; the coordinates differ per faction stream.
                    let (dx, dy) = if jitter == 0 {
                        (0, 0)
                    } else {
                        (
                            self.rng.uniform(2 * jitter as u32 + 1) as i32 - jitter,
                            self.rng.uniform(2 * jitter as u32 + 1) as i32 - jitter,
                        )
                    };
                    (
                        agent,
                        bounds(Coord::new(portal.position.x + dx, portal.position.y + dy)),
                    )
                })
                .collect(),
            None => {
                let count = map.len() as u32;
                let offset = self.rng.uniform(count.max(1));
                roster
                    .iter()
                    .enumerate()
                    .map(|(index, &agent)| {
                        let portal = map
                            .portal(PortalId((offset as usize + index) as u32 % count))
                            .expect("dense portal ids");
                        (agent, portal.position)
                    })
                    .collect()
            }
        }
    }

    fn status(&self, plan: &FieldCandidate, obs: &Observation) -> PlanStatus {
        let [a, b, c] = plan.vertices;
        let enemy = owners(self.faction).1;
        let mut complete = true;
        for (source, target) in [(a, b), (a, c), (b, c)] {
            let link_id =
                arena_core::types::LinkId::new(source, target).expect("distinct plan vertices");
            match obs.links.iter().find(|l| l.id == link_id) {
                Some(link) if link.faction == self.faction => continue,
                Some(_) => return PlanStatus::Invalid,
                None => {
                    complete = false;
                    let owner_of = |id: PortalId| common::portal(obs, id).owner;
                    if owner_of(source) == enemy || owner_of(target) == enemy {
                        return PlanStatus::Invalid;
                    }
                    let (ps, pt) = (
                        common::portal(obs, source).position,
                        common::portal(obs, target).position,
                    );
                    let crossed = obs.links.iter().any(|l| {
                        let (u, v) = (
                            common::portal(obs, l.id.min).position,
                            common::portal(obs, l.id.max).position,
                        );
                        let shared = l.id.contains(source) || l.id.contains(target);
                        !shared && arena_core::geometry::segments_intersect(ps, pt, u, v)
                    });
                    if crossed {
                        return PlanStatus::Invalid;
                    }
                }
            }
        }
        if complete {
            PlanStatus::Complete
        } else {
            PlanStatus::Active
        }
    }

    /// The replan discipline (frozen `FieldPlanner` semantics): COMPLETE and
    /// INVALID replan immediately; only a FAILED search is throttled.
    pub fn refresh(&mut self, obs: &Observation) {
        let status = match &self.plan {
            Some(plan) => self.status(plan, obs),
            None => PlanStatus::Invalid,
        };
        match status {
            PlanStatus::Active => self.active = self.plan,
            PlanStatus::Complete | PlanStatus::Invalid => {
                let due = status == PlanStatus::Complete || obs.round >= self.next_search_round;
                if due {
                    let ranked =
                        rank_field_candidates(obs, self.faction, &self.cfg, &self.params.value);
                    self.best_pending_area2 = ranked.first().map(|(_, c)| c.area2).unwrap_or(0);
                    match pick_near_equal(
                        &ranked,
                        |(value, _)| *value,
                        self.params.near_equal_permille,
                        &mut self.rng,
                    )
                    .map(|(_, candidate)| *candidate)
                    {
                        Some(candidate) => {
                            self.plan = Some(candidate);
                            self.next_search_round = obs.round;
                        }
                        None => {
                            self.plan = None;
                            self.next_search_round = obs.round + self.params.replan_retry_rounds;
                        }
                    }
                }
                self.active = match &self.plan {
                    Some(plan) if self.status(plan, obs) == PlanStatus::Active => Some(*plan),
                    _ => None,
                };
            }
        }
        self.refresh_second(obs);
    }

    /// The second-plan discipline: in plan mode with the discount
    /// enabled, keep a SECOND active candidate for the agents the primary
    /// triangle cannot employ. COMPLETE (the surplus built it) and INVALID
    /// re-derive immediately; a failed search is throttled by the same
    /// retry as the primary. Fallback mode clears it (the fallback spread
    /// already serves every agent). Deterministic: the best non-crossing
    /// candidate by (value, vertices) — no RNG, so mirror factions stay
    /// seed-symmetric.
    fn refresh_second(&mut self, obs: &Observation) {
        if self.params.second_plan_discount_permille == 0 || self.active.is_none() {
            self.second = None;
            self.next_second_search_round = obs.round;
            return;
        }
        let second_active = self
            .second
            .is_some_and(|second| self.status(&second, obs) == PlanStatus::Active);
        if second_active && self.second != self.active {
            return;
        }
        if obs.round < self.next_second_search_round {
            return;
        }
        let ranked = rank_field_candidates(obs, self.faction, &self.cfg, &self.params.value);
        let picked = ranked
            .iter()
            .filter(|(_, candidate)| {
                Some(candidate) != self.active.as_ref() && !self.crosses_plan(obs, candidate)
            })
            .find(|(_, candidate)| self.status(candidate, obs) == PlanStatus::Active)
            .map(|(_, candidate)| *candidate);
        self.second = picked;
        if picked.is_none() {
            self.next_second_search_round = obs.round + self.params.replan_retry_rounds;
        }
    }

    /// Whether `candidate`'s pending edges cross any pending edge of the
    /// ACTIVE plan (shared endpoints exempt — a shared vertex anchors both
    /// triangles). Existing links cannot be crossed by legality, so only
    /// the two plans' pending edges interact.
    fn crosses_plan(&self, obs: &Observation, candidate: &FieldCandidate) -> bool {
        let Some(plan) = &self.active else {
            return false;
        };
        for (cs, ct) in Self::edges(&candidate.vertices) {
            let candidate_link = arena_core::types::LinkId::new(cs, ct);
            let fired = obs.links.iter().any(|l| Some(l.id) == candidate_link);
            if fired {
                continue;
            }
            let (ps, pt) = (
                common::portal(obs, cs).position,
                common::portal(obs, ct).position,
            );
            for (ss, st) in Self::edges(&plan.vertices) {
                let plan_link = arena_core::types::LinkId::new(ss, st);
                let plan_fired = obs.links.iter().any(|l| Some(l.id) == plan_link);
                if plan_fired || cs == ss || cs == st || ct == ss || ct == st {
                    continue;
                }
                let (qs, qt) = (
                    common::portal(obs, ss).position,
                    common::portal(obs, st).position,
                );
                if arena_core::geometry::segments_intersect(ps, pt, qs, qt) {
                    return true;
                }
            }
        }
        false
    }

    /// The plan validated for this round (`None` = fallback mode).
    pub fn active(&self) -> Option<&FieldCandidate> {
        self.active.as_ref()
    }

    /// The faction's economy at `obs`: horizon from the value model,
    /// opportunity rate = the best pending own candidate's per-round income.
    pub fn economy(&self, obs: &Observation) -> Economy {
        Economy::for_round(
            &self.cfg,
            obs.round,
            &self.params.value,
            self.best_pending_area2,
        )
    }

    /// Marginal priority of assigning `me` to `station`: the plan's net
    /// value minus the candidate's own income for every round the agent
    /// spends reaching the station (integer score2 units, saturating).
    fn station_priority(
        &self,
        obs: &Observation,
        base: i64,
        per_round: u64,
        me: &arena_core::observe::AgentPrivate,
        station: PortalId,
    ) -> i64 {
        let target = common::portal(obs, station);
        let rounds = crate::value::travel_rounds_between(
            me.position,
            target.position,
            self.cfg.move_budget_cm,
        );
        base.saturating_sub((per_round as i128 * rounds as i128).min(i64::MAX as i128) as i64)
    }

    /// The build task requests of the ACTIVE plan for every own agent:
    /// vertex stations (capture the neutral, key-farm the keyless own) and
    /// edge stations (link when both vertices are ours and the agent holds
    /// the target key), each agent ranked by net marginal priority. An
    /// agent requests several tasks so the allocator can fall through when
    /// its preferred station is taken — the staggering that removes
    /// same-portal crowding. With a second plan active, the same
    /// stations of the SECOND candidate are appended at the recorded
    /// discount — surplus agents (and reinforcements) fall through into
    /// them after the primary plan's work, instead of idling.
    pub fn build_requests(&self, obs: &Observation) -> BTreeMap<AgentId, Vec<TaskRequest>> {
        let mut requests = BTreeMap::new();
        let Some(plan) = &self.active else {
            return requests;
        };
        let base = field_value(obs, self.faction, &self.cfg, &self.params.value, plan);
        let per_round = plan.area2;
        // The second plan's discounted base: its stations rank strictly
        // below comparable primary stations but far above any fallback. A
        // non-positive discounted base requests nothing (surplus agents
        // idle rather than build a candidate whose setup outlasts its
        // value).
        let second_base = self.second.map_or(0, |candidate| {
            let raw = field_value(obs, self.faction, &self.cfg, &self.params.value, &candidate);
            (raw as i128 * self.params.second_plan_discount_permille as i128 / 1000)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64
        });
        let second = match (&self.second, second_base > 0) {
            (Some(second), true) => Some(*second),
            _ => None,
        };
        let second_per_round = second.map_or(0, |candidate| candidate.area2);
        let keyed = |portal: PortalId| {
            obs.own_agents
                .iter()
                .any(|agent| agent.keys.get(&portal).copied().unwrap_or(0) >= 1)
        };
        let mut missing_key_vertices = std::collections::BTreeSet::new();
        for candidate in [Some(plan), second.as_ref()].into_iter().flatten() {
            for (source, target) in Self::edges(&candidate.vertices) {
                if obs
                    .links
                    .iter()
                    .any(|link| link.id == arena_core::types::LinkId::new(source, target).unwrap())
                {
                    continue;
                }
                let owned = [source, target]
                    .into_iter()
                    .all(|portal| common::portal(obs, portal).owner == self.own);
                let no_usable_holder = owned
                    && !obs.own_agents.iter().any(|agent| {
                        [(source, target), (target, source)]
                            .into_iter()
                            .any(|(s, t)| {
                                common::link_completion_delay(obs, self.own, &self.cfg, agent, s, t)
                                    .is_some()
                            })
                    });
                if (!keyed(source) && !keyed(target)) || no_usable_holder {
                    missing_key_vertices.extend([source, target]);
                }
            }
        }
        for me in obs.own_agents.iter() {
            let mut mine = Vec::new();
            for (candidate, base, per_round) in [
                (Some(plan), base, per_round),
                (second.as_ref(), second_base, second_per_round),
            ] {
                let Some(candidate) = candidate else {
                    continue;
                };
                for &vertex in &candidate.vertices {
                    let view = common::portal(obs, vertex);
                    match view.owner {
                        arena_core::types::Owner::Neutral => {
                            let pri = self.station_priority(obs, base, per_round, me, vertex);
                            mine.push(TaskRequest::one_shot(Task::Capture { portal: vertex }, pri));
                        }
                        owner
                            if owner == self.own
                                && missing_key_vertices.contains(&vertex)
                                && me.keys.get(&vertex).copied().unwrap_or(0) == 0 =>
                        {
                            let pri = self.station_priority(obs, base, per_round, me, vertex);
                            mine.push(TaskRequest::one_shot(Task::KeyFarm { portal: vertex }, pri));
                        }
                        _ => {}
                    }
                }
                for (source, target) in Self::edges(&candidate.vertices) {
                    let link_id = arena_core::types::LinkId::new(source, target)
                        .expect("distinct plan vertices");
                    let exists = obs
                        .links
                        .iter()
                        .any(|l| l.id == link_id && l.faction == self.faction);
                    if exists {
                        continue;
                    }
                    let (view_s, view_t) =
                        (common::portal(obs, source), common::portal(obs, target));
                    if view_s.owner != self.own || view_t.owner != self.own {
                        continue;
                    }
                    for (source, target) in [(source, target), (target, source)] {
                        if let Some(delay) = common::link_completion_delay(
                            obs, self.own, &self.cfg, me, source, target,
                        ) {
                            let pri = base.saturating_sub(
                                (per_round as i128 * delay as i128).min(i64::MAX as i128) as i64,
                            );
                            mine.push(TaskRequest::one_shot(Task::Link { source, target }, pri));
                        }
                    }
                }
            }
            requests.insert(me.id, mine);
        }
        requests
    }

    /// The three canonical edges of a triangle (ascending pairs).
    fn edges(vertices: &[PortalId; 3]) -> [(PortalId, PortalId); 3] {
        let [a, b, c] = *vertices;
        [
            (a.min(b), a.max(b)),
            (a.min(c), a.max(c)),
            (b.min(c), b.max(c)),
        ]
    }

    /// Fallback task requests (no viable plan anywhere): every agent
    /// requests its nearest incomplete portals (positive priorities well
    /// below any live-plan value, so a replan preempts fallback instantly)
    /// plus every opportunistic LINK it could legally emit right now — the
    /// fallback still closes triangles, it just has no plan to walk.
    pub fn fallback_requests(&self, obs: &Observation) -> BTreeMap<AgentId, Vec<TaskRequest>> {
        let mut requests = BTreeMap::new();
        for me in obs.own_agents.iter() {
            let mut mine = Vec::new();
            // Opportunistic links first (the highest-value fallback work).
            for (source, target) in self.own_pairs_this_round(obs, me) {
                mine.push(TaskRequest::one_shot(
                    Task::Link { source, target },
                    self.params.fallback_priority,
                ));
            }
            // Nearest incomplete portals, near-first.
            let mut incomplete: Vec<(i64, PortalId)> = obs
                .portals
                .iter()
                .filter(|p| {
                    p.owner == arena_core::types::Owner::Neutral
                        || me.keys.get(&p.id).copied().unwrap_or(0) == 0
                })
                .map(|p| (arena_core::geometry::dist2(me.position, p.position), p.id))
                .collect();
            incomplete.sort();
            for (rank, (dist2, portal)) in incomplete.into_iter().take(3).enumerate() {
                let view = common::portal(obs, portal);
                let task = match view.owner {
                    arena_core::types::Owner::Neutral => Task::Capture { portal },
                    _ => Task::KeyFarm { portal },
                };
                let _ = dist2;
                mine.push(TaskRequest::one_shot(
                    task,
                    self.params
                        .fallback_priority
                        .saturating_sub(1 + rank as i64 + (rank as i64) * rank as i64),
                ));
            }
            requests.insert(me.id, mine);
        }
        requests
    }

    /// Every legal LINK `(source, target)` `me` could emit this round:
    /// standing or moving into operation range of an own `source`, holding a key of
    /// an own `target`, and passing the full pre-flight legality check.
    fn own_pairs_this_round(
        &self,
        obs: &Observation,
        me: &arena_core::observe::AgentPrivate,
    ) -> Vec<(PortalId, PortalId)> {
        let mut pairs = Vec::new();
        for source in obs.portals.iter().filter(|p| p.owner == self.own) {
            let mut targets: Vec<PortalId> = me
                .keys
                .iter()
                .filter(|(portal, count)| {
                    **count >= 1
                        && **portal != source.id
                        && common::portal(obs, **portal).owner == self.own
                })
                .map(|(&portal, _)| portal)
                .collect();
            targets.sort();
            for target in targets {
                if common::link_position_this_round(obs, self.own, &self.cfg, me, source.id, target)
                    .is_some()
                {
                    pairs.push((source.id, target));
                }
            }
        }
        pairs
    }

    /// Executes the granted tasks into command lists, with the
    /// observation-level pre-submit legality checks (execution
    /// races still record as normal Drops): approach stations with one
    /// Move, act when in range and the observation still supports the
    /// action. This preview is pure; Hack is booked only on emission.
    pub(super) fn commands_for(
        &self,
        obs: &Observation,
        me: &arena_core::observe::AgentPrivate,
        task: Task,
    ) -> CommandList {
        let round = obs.round;
        let cfg = &self.cfg;
        let own = self.own;
        let mut commands = CommandList::new();
        match task {
            Task::Capture { portal } => {
                let target = common::portal(obs, portal);
                if target.owner == arena_core::types::Owner::Neutral
                    && in_range(me.position, target.position, cfg.portal_operation_range_cm)
                {
                    commands.major(arena_core::types::Command::Capture { portal });
                } else if target.owner != own {
                    commands.mv(arena_core::types::Command::Move {
                        destination: step_toward(me.position, target.position, cfg.move_budget_cm),
                    });
                } else {
                    // Captured by a teammate in the same round's
                    // race or an earlier round: farm a key instead.
                    if me.keys.get(&portal).copied().unwrap_or(0) == 0
                        && in_range(me.position, target.position, cfg.portal_operation_range_cm)
                        && self.ledger.available(round, cfg, me.id, portal)
                    {
                        commands.aux(arena_core::types::Command::Hack { portal });
                    }
                }
            }
            Task::KeyFarm { portal } => {
                let target = common::portal(obs, portal);
                if in_range(me.position, target.position, cfg.portal_operation_range_cm) {
                    if self.ledger.available(round, cfg, me.id, portal) {
                        commands.aux(arena_core::types::Command::Hack { portal });
                    }
                } else {
                    commands.mv(arena_core::types::Command::Move {
                        destination: step_toward(me.position, target.position, cfg.move_budget_cm),
                    });
                }
            }
            Task::Link { source, target } => {
                let stand = common::portal(obs, source);
                if let Some(position) =
                    common::link_position_this_round(obs, own, cfg, me, source, target)
                {
                    if position != me.position {
                        commands.aux(arena_core::types::Command::Move {
                            destination: position,
                        });
                    }
                    commands.aux(arena_core::types::Command::Link {
                        source_portal: source,
                        target_portal: target,
                    });
                } else if common::portal(obs, source).owner == own
                    || common::portal(obs, source).owner == arena_core::types::Owner::Neutral
                {
                    commands.mv(arena_core::types::Command::Move {
                        destination: step_toward(me.position, stand.position, cfg.move_budget_cm),
                    });
                }
            }
            Task::Attack { portal } => {
                let target = common::portal(obs, portal);
                let enemy = owners(self.faction).1;
                if target.owner == enemy
                    && in_range(me.position, target.position, cfg.portal_operation_range_cm)
                {
                    commands.major(arena_core::types::Command::Attack);
                } else if target.owner == enemy {
                    let destination = step_toward(me.position, target.position, cfg.move_budget_cm);
                    // Aux Move precedes the Major Action in the same stream.
                    if in_range(destination, target.position, cfg.portal_operation_range_cm) {
                        commands.aux(arena_core::types::Command::Move { destination });
                        commands.major(arena_core::types::Command::Attack);
                    } else {
                        commands.mv(arena_core::types::Command::Move { destination });
                    }
                }
            }
            Task::Recharge { portal } => {
                let target = common::portal(obs, portal);
                // The overlay releases full-HP anchors. A stale grant
                // also emits nothing; remote target Keys satisfy the
                // position condition without being consumed.
                let damaged = target.hp < cfg.portal_max_hp;
                if target.owner == own
                    && damaged
                    && (in_range(me.position, target.position, cfg.portal_operation_range_cm)
                        || me.keys.get(&portal).copied().unwrap_or(0) > 0)
                    && me.supply >= cfg.recharge_cost_supply
                {
                    commands.major(arena_core::types::Command::Recharge { portal });
                } else if target.owner == own && damaged && me.supply >= cfg.recharge_cost_supply {
                    let destination = step_toward(me.position, target.position, cfg.move_budget_cm);
                    if in_range(destination, target.position, cfg.portal_operation_range_cm) {
                        commands.aux(arena_core::types::Command::Move { destination });
                        commands.major(arena_core::types::Command::Recharge { portal });
                    } else {
                        commands.mv(arena_core::types::Command::Move { destination });
                    }
                }
            }
        }
        commands
    }

    fn record_hacks(
        &mut self,
        round: u32,
        agent: AgentId,
        commands: &[arena_core::types::Command],
    ) {
        for command in commands {
            if let arena_core::types::Command::Hack { portal } = command {
                self.ledger.record(round, &self.cfg, agent, *portal);
            }
        }
    }

    pub(super) fn hack_available(&self, round: u32, agent: AgentId, portal: PortalId) -> bool {
        self.ledger.available(round, &self.cfg, agent, portal)
    }

    fn companion_grants(
        &self,
        obs: &Observation,
        board: &mut TaskBoard,
        primary: &BTreeMap<AgentId, Task>,
        ready: &BTreeMap<AgentId, Vec<TaskRequest>>,
    ) -> BTreeMap<AgentId, Task> {
        let requests = obs
            .own_agents
            .iter()
            .map(|me| {
                let candidates = ready
                    .get(&me.id)
                    .into_iter()
                    .flatten()
                    .filter(|request| {
                        primary.get(&me.id).is_some_and(|&task| {
                            crate::compound::pair(self, obs, me, task, request.task).is_some()
                        })
                    })
                    .copied()
                    .collect();
                (me.id, candidates)
            })
            .collect();
        let agents: Vec<_> = primary.keys().copied().collect();
        allocate_compatible(board, &agents, &requests, |agent, task, companions| {
            let mut geometry = primary.clone();
            for (&id, &companion) in companions {
                if matches!(companion, Task::Link { .. }) {
                    geometry.insert(id, companion);
                }
            }
            self.compatible_link_grant(obs, agent, task, &geometry)
        })
    }

    /// Links emitted this round share a geometric reservation as well as
    /// an edge id. Opponent races remain unknown; our own crossing edges
    /// are known before submission and must fall through to alternate work.
    fn compatible_link_grant(
        &self,
        obs: &Observation,
        agent: AgentId,
        task: Task,
        granted: &BTreeMap<AgentId, Task>,
    ) -> bool {
        let Task::Link { source, target } = task else {
            return true;
        };
        let me = obs.own_agents.iter().find(|me| me.id == agent).unwrap();
        if common::link_position_this_round(obs, self.own, &self.cfg, me, source, target).is_none()
        {
            return true;
        }
        granted.iter().all(|(&other, task)| {
            let Task::Link {
                source: other_source,
                target: other_target,
            } = *task
            else {
                return true;
            };
            if [source, target]
                .iter()
                .any(|p| *p == other_source || *p == other_target)
            {
                return true;
            }
            let teammate = obs.own_agents.iter().find(|me| me.id == other).unwrap();
            common::link_position_this_round(
                obs,
                self.own,
                &self.cfg,
                teammate,
                other_source,
                other_target,
            )
            .is_none()
                || !arena_core::geometry::segments_intersect(
                    common::portal(obs, source).position,
                    common::portal(obs, target).position,
                    common::portal(obs, other_source).position,
                    common::portal(obs, other_target).position,
                )
        })
    }

    /// Allocate compatible work, then recheck the actual combat cohorts.
    /// Availability and surplus work are evaluated on the same reservation board.
    fn preview_allocation(
        &self,
        obs: &Observation,
        requests: &BTreeMap<AgentId, Vec<TaskRequest>>,
    ) -> RoundAllocation {
        let agents: Vec<_> = obs.own_agents.iter().map(|a| a.id).collect();
        let mut ready: BTreeMap<AgentId, Vec<TaskRequest>> = obs
            .own_agents
            .iter()
            .map(|agent| {
                let mine = requests
                    .get(&agent.id)
                    .into_iter()
                    .flatten()
                    .filter(|request| match request.task {
                        Task::KeyFarm { portal } => {
                            !in_range(
                                agent.position,
                                common::portal(obs, portal).position,
                                self.cfg.portal_operation_range_cm,
                            ) || self
                                .ledger
                                .available(obs.round, &self.cfg, agent.id, portal)
                        }
                        _ => true,
                    })
                    .copied()
                    .collect();
                (agent.id, mine)
            })
            .collect();
        let mut board = TaskBoard::new();
        let compatible = |agent, task, granted: &BTreeMap<AgentId, Task>| {
            self.compatible_link_grant(obs, agent, task, granted)
        };
        let mut grants = allocate_compatible(&mut board, &agents, &ready, compatible);
        let mut companions;
        // Construction or another combat task may win a required teammate.
        // Recheck actual cohorts until removing a failed plan no longer
        // changes the others. Each iteration removes at least one request.
        loop {
            companions = self.companion_grants(obs, &mut board, &grants, &ready);
            let combat_grants: BTreeMap<_, _> = grants
                .iter()
                .chain(companions.iter())
                .filter(|(_, task)| crate::compound::is_combat(**task))
                .map(|(&agent, &task)| (agent, task))
                .collect();
            let mut rejected = std::collections::BTreeSet::new();
            let targets: std::collections::BTreeSet<_> = combat_grants
                .values()
                .filter_map(|task| {
                    if let Task::Attack { portal } = task {
                        Some(*portal)
                    } else {
                        None
                    }
                })
                .collect();
            for portal in targets {
                let target = common::portal(obs, portal);
                let economy = self.economy(obs);
                let team: Vec<_> = obs
                    .own_agents
                    .iter()
                    .filter(|agent| combat_grants.get(&agent.id) == Some(&Task::Attack { portal }))
                    .filter_map(|agent| {
                        crate::value::AttackArrival::for_agent(
                            agent,
                            target.position,
                            &self.cfg,
                            economy.horizon_rounds,
                        )
                    })
                    .collect();
                let income = crate::value::attack_income_per_round(
                    obs,
                    self.faction,
                    &self.cfg,
                    &self.params.value,
                    portal,
                );
                if crate::value::strike_value(
                    &economy,
                    income,
                    crate::value::attacks_needed(target.hp, &self.cfg),
                    &team,
                )
                .is_none_or(|value| value <= 0)
                {
                    rejected.insert(Task::Attack { portal });
                }
            }
            let rescue_targets: std::collections::BTreeSet<_> = combat_grants
                .values()
                .filter_map(|task| match task {
                    Task::Recharge { portal } => Some(*portal),
                    _ => None,
                })
                .collect();
            if !rescue_targets.is_empty() {
                let economy = self.economy(obs);
                let defense = crate::value::DefenseModel::new(
                    obs,
                    self.faction,
                    &self.cfg,
                    &self.params.value,
                    &economy,
                );
                for portal in rescue_targets {
                    let team: Vec<_> = combat_grants
                        .iter()
                        .filter_map(|(&agent, task)| {
                            (*task == Task::Recharge { portal }).then_some(agent)
                        })
                        .collect();
                    if defense.coordinated_value(&team, portal) <= 0 {
                        rejected.insert(Task::Recharge { portal });
                    }
                }
            }
            if rejected.is_empty() {
                break;
            }
            for mine in ready.values_mut() {
                mine.retain(|request| !rejected.contains(&request.task));
            }
            board = TaskBoard::new();
            grants = allocate_compatible(&mut board, &agents, &ready, compatible);
        }
        // Unused builders may expand without drawing a crossing Link through
        // either live plan. Reuse the same reservation board; planned work and
        // policy overlays keep their grants, and no capture is duplicated.
        {
            let free: Vec<_> = agents
                .iter()
                .copied()
                .filter(|agent| !grants.contains_key(agent))
                .collect();
            let mut expansion = BTreeMap::new();
            for &agent in &free {
                let me = obs.own_agents.iter().find(|me| me.id == agent).unwrap();
                let mut targets: Vec<_> = obs
                    .portals
                    .iter()
                    .filter(|p| p.owner == arena_core::types::Owner::Neutral)
                    .filter_map(|p| {
                        common::movement_rounds_to_within(
                            me.position,
                            p.position,
                            self.cfg.portal_operation_range_cm,
                            self.cfg.move_budget_cm,
                            self.cfg.total_rounds.saturating_sub(obs.round),
                        )
                        .map(|moves| (moves, p.id))
                    })
                    .collect();
                targets.sort();
                let mine: Vec<_> = targets
                    .into_iter()
                    .map(|(moves, portal)| {
                        TaskRequest::one_shot(Task::Capture { portal }, -(moves as i64))
                    })
                    .collect();
                for request in &mine {
                    if !ready[&agent].iter().any(|r| r.task == request.task) {
                        ready.get_mut(&agent).unwrap().push(*request);
                    }
                }
                expansion.insert(agent, mine);
            }
            grants.extend(allocate(&mut board, &free, &expansion));
        }
        RoundAllocation {
            agents: agents
                .iter()
                .map(|&agent| AgentDecision {
                    agent,
                    grant: grants.get(&agent).copied(),
                    companion: companions.get(&agent).copied(),
                })
                .collect(),
        }
    }

    /// Allocate work and execute it with compatible auxiliary actions.
    pub fn allocate_and_execute(
        &mut self,
        obs: &Observation,
        requests: &BTreeMap<AgentId, Vec<TaskRequest>>,
    ) -> BTreeMap<AgentId, CommandList> {
        let decision = self.preview_allocation(obs, requests);
        let mut out = BTreeMap::new();
        for agent in &decision.agents {
            let Some(task) = agent.grant else {
                continue;
            };
            let me = obs
                .own_agents
                .iter()
                .find(|me| me.id == agent.agent)
                .unwrap();
            if crate::compound::is_combat(task)
                || agent.companion.is_some_and(crate::compound::is_combat)
            {
                let mut commands = agent.companion.map_or_else(
                    || self.commands_for(obs, me, task).build(),
                    |companion| {
                        crate::compound::pair(self, obs, me, task, companion)
                            .expect("the allocation preview validated this exact pair")
                    },
                );
                crate::compound::add_hack(self, obs, me, &mut commands);
                self.record_hacks(obs.round, agent.agent, &commands);
                if !commands.is_empty() {
                    out.insert(agent.agent, CommandList::from_ordered(commands));
                }
            } else {
                let commands = self.commands_for(obs, me, task);
                self.record_hacks(obs.round, agent.agent, &commands.clone().build());
                if !commands.is_empty() {
                    out.insert(agent.agent, commands);
                }
            }
        }
        self.replenish(obs, &decision, &mut out);
        out
    }

    fn replenish(
        &mut self,
        obs: &Observation,
        decision: &RoundAllocation,
        out: &mut BTreeMap<AgentId, CommandList>,
    ) {
        if self.params.supply_reserve_recharges == 0 {
            return;
        }
        for agent in &decision.agents {
            let me = obs
                .own_agents
                .iter()
                .find(|me| me.id == agent.agent)
                .unwrap();
            let original = out
                .get(&agent.agent)
                .cloned()
                .map_or_else(Vec::new, CommandList::build);
            let mut commands = original.clone();
            crate::compound::replenish(self, obs, me, agent.grant.is_none(), &mut commands);
            if commands != original {
                // replenish never changes a stream that already contains a
                // Hack, so this records only the newly emitted harvest.
                self.record_hacks(obs.round, agent.agent, &commands);
                out.insert(agent.agent, CommandList::from_ordered(commands));
            }
        }
    }

    /// The faction this core plays.
    pub fn faction(&self) -> Faction {
        self.faction
    }

    /// The match's GameConfig (the value-model helpers need it).
    pub fn cfg(&self) -> &GameConfig {
        &self.cfg
    }

    /// The core's heuristic parameters.
    pub fn params(&self) -> &CoreParams {
        &self.params
    }
}
