//! Integer movement and link checks, Hack cooldown memory, and ordered commands.

use std::collections::BTreeMap;

use arena_core::config::GameConfig;
use arena_core::geometry::{
    ceil_integer_sqrt, dist2, point_on_segment, segments_intersect, within_closed_range,
};
use arena_core::harness::AgentSubmission;
use arena_core::observe::{AgentPrivate, Observation, PortalPublic};
use arena_core::state::GameState;
use arena_core::types::{AgentId, Command, Coord, Owner, PortalId};

/// Closed-range check (`dist2 <= range^2`) over promoted i64 coordinates.
pub fn in_range(a: Coord, b: Coord, range_cm: u32) -> bool {
    within_closed_range(a, b, range_cm)
}

/// A legal source position reachable in this round, including a Move
/// followed by Link in the same ordered auxiliary command stream.
pub fn link_position_this_round(
    obs: &Observation,
    own: Owner,
    cfg: &GameConfig,
    agent: &AgentPrivate,
    source: PortalId,
    target: PortalId,
) -> Option<Coord> {
    if link_is_legal(obs, own, cfg, agent.position, &agent.keys, source, target) {
        return Some(agent.position);
    }
    let position = step_toward(
        agent.position,
        portal(obs, source).position,
        cfg.move_budget_cm,
    );
    link_is_legal(obs, own, cfg, position, &agent.keys, source, target).then_some(position)
}

/// Earliest future decision offset for the same Move-then-Link executor,
/// assuming the observed ownership and link geometry remain unchanged.
pub fn link_completion_delay(
    obs: &Observation,
    own: Owner,
    cfg: &GameConfig,
    agent: &AgentPrivate,
    source: PortalId,
    target: PortalId,
) -> Option<u32> {
    let stand = portal(obs, source).position;
    if !link_is_legal(obs, own, cfg, stand, &agent.keys, source, target) {
        return None;
    }
    let mut position = agent.position;
    for delay in 0..cfg.total_rounds.saturating_sub(obs.round).saturating_add(1) {
        if link_is_legal(obs, own, cfg, position, &agent.keys, source, target) {
            return Some(delay);
        }
        let next = step_toward(position, stand, cfg.move_budget_cm);
        if next == position {
            return None;
        }
        position = next;
        if in_range(position, stand, cfg.portal_operation_range_cm) {
            return Some(delay);
        }
    }
    None
}

/// The faction's `(own, enemy)` portal owners.
pub fn owners(faction: arena_core::types::Faction) -> (Owner, Owner) {
    match faction {
        arena_core::types::Faction::Enl => (Owner::Enl, Owner::Res),
        arena_core::types::Faction::Res => (Owner::Res, Owner::Enl),
    }
}

/// Integer `ceil_integer_sqrt` stepping toward `to` capped at one round's
/// `MOVE_BUDGET` (the AggroBot pattern): the full destination when it is
/// within budget, else a budget-length axis-proportional step. The result
/// stays on the segment, so it is in bounds whenever both endpoints are.
pub fn step_toward(from: Coord, to: Coord, budget_cm: u32) -> Coord {
    let dx = to.x as i64 - from.x as i64;
    let dy = to.y as i64 - from.y as i64;
    let dist = ceil_integer_sqrt((dx * dx + dy * dy) as u64);
    if dist <= budget_cm as u64 {
        return to;
    }
    let dist = dist as i64;
    let budget = budget_cm as i64;
    let step = |delta: i64| ((delta.abs() * budget / dist) * delta.signum()) as i32;
    let (mut sx, mut sy) = (step(dx), step(dy));
    if sx == 0 && sy == 0 {
        // Unreachable while dist > budget (the dominant axis steps by at
        // least budget / 2), but stay total: nudge the dominant axis.
        if dx.abs() >= dy.abs() {
            sx = dx.signum() as i32;
        } else {
            sy = dy.signum() as i32;
        }
    }
    Coord::new(from.x + sx, from.y + sy)
}

/// Moves needed to enter the operation range using the executor's exact
/// integer stepping. Returns None if the remaining decision window is too short.
pub fn movement_rounds_to_within(
    from: Coord,
    to: Coord,
    range_cm: u32,
    budget_cm: u32,
    max_rounds: u32,
) -> Option<u32> {
    let mut position = from;
    for moves in 0..=max_rounds {
        if in_range(position, to, range_cm) {
            return Some(moves);
        }
        if budget_cm == 0 || moves == max_rounds {
            return None;
        }
        position = step_toward(position, to, budget_cm);
    }
    None
}

/// Own-memory mirror of the engine's hack trackers: hack cooldown and the
/// per-epoch limit are NOT part of the `Observation`, so a bot that wants
/// near-zero Hack DROPs books its own successful submissions per
/// `(agent, portal)` and resets them at every epoch boundary exactly like
/// the eager sweep of D32 (`GameState::epoch_index`).
#[derive(Clone, Default, Debug)]
pub struct HackLedger {
    epoch: u32,
    entries: BTreeMap<(AgentId, PortalId), HackEntry>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct HackEntry {
    pub last_round: Option<u32>,
    pub count_epoch: u32,
}

impl HackLedger {
    pub fn new() -> Self {
        HackLedger::default()
    }

    /// Eager epoch sweep (D32): on a boundary, every tracker resets
    /// `last_hack_round` and `hack_count_epoch`.
    fn sweep(&mut self, round: u32, epoch_length: u32) {
        let epoch = GameState::epoch_index(round, epoch_length);
        if epoch != self.epoch {
            self.entries.clear();
            self.epoch = epoch;
        }
    }

    /// Read-only eligibility for planning, including the impending epoch reset.
    pub fn available(
        &self,
        round: u32,
        cfg: &GameConfig,
        agent: AgentId,
        portal: PortalId,
    ) -> bool {
        if GameState::epoch_index(round, cfg.epoch_length) != self.epoch {
            return true;
        }
        match self.entries.get(&(agent, portal)) {
            None => true,
            Some(entry) => {
                if let Some(last) = entry.last_round {
                    if round.saturating_sub(last) < cfg.hack_cooldown_rounds {
                        return false;
                    }
                }
                entry.count_epoch < cfg.hack_limit_per_epoch
            }
        }
    }

    /// Books an emitted HACK optimistically: the bot never re-reads the
    /// engine's tracker, so a hack that raced and dropped only makes the
    /// ledger more conservative.
    pub fn record(&mut self, round: u32, cfg: &GameConfig, agent: AgentId, portal: PortalId) {
        self.sweep(round, cfg.epoch_length);
        let entry = self.entries.entry((agent, portal)).or_default();
        entry.last_round = Some(round);
        entry.count_epoch += 1;
    }
}

/// A validated command stream. Ordinary tasks order auxiliary actions,
/// one major action, then movement. Compound tasks use `from_ordered`
/// to preserve their explicit Recharge / Move / Link / Hack sequence.
#[derive(Clone, Default, Debug)]
pub struct CommandList {
    aux: Vec<Command>,
    major: Option<Command>,
    after_major: Vec<Command>,
    mv: Option<Command>,
}

impl CommandList {
    pub fn new() -> Self {
        CommandList::default()
    }

    /// Preserve an explicitly planned order, including auxiliary work after
    /// a Major Action (e.g. Recharge before spending its last remote Key).
    pub fn from_ordered(commands: Vec<Command>) -> Self {
        let mut result = Self::new();
        for command in commands {
            if command.is_major() {
                assert!(result.major.is_none(), "one major action per stream");
                result.major = Some(command);
            } else if result.major.is_some() {
                result.after_major.push(command);
            } else {
                result.aux.push(command);
            }
        }
        result
    }

    /// Appends an auxiliary (HACK-class) command; they run first, in
    /// insertion order.
    pub fn aux(&mut self, command: Command) {
        self.aux.push(command);
    }

    /// Sets the (at most one) Major Action; after every auxiliary.
    pub fn major(&mut self, command: Command) {
        self.major = Some(command);
    }

    /// Sets the (at most one) Move; always last.
    pub fn mv(&mut self, command: Command) {
        self.mv = Some(command);
    }

    pub fn is_empty(&self) -> bool {
        self.aux.is_empty()
            && self.major.is_none()
            && self.after_major.is_empty()
            && self.mv.is_none()
    }

    /// Emits the ordered command Vec (empty Vec = explicit PASS stream).
    pub fn build(self) -> Vec<Command> {
        let mut commands = self.aux;
        if let Some(command) = self.major {
            commands.push(command);
        }
        commands.extend(self.after_major);
        if let Some(command) = self.mv {
            commands.push(command);
        }
        commands
    }
}

/// Builds an ordered stream. Offline tests use bucket 0; the SDK sends
/// only the commands and the server timestamps their actual arrival.
pub fn submission(agent: AgentId, commands: Vec<Command>) -> AgentSubmission {
    AgentSubmission::new(agent, commands)
}

/// Looks one portal up by id (observation portals are dense by map
/// construction, so a `None` is a bot-side bug).
pub fn portal(obs: &Observation, id: PortalId) -> &PortalPublic {
    obs.portals
        .iter()
        .find(|p| p.id == id)
        .expect("portal id comes from the same observation")
}

/// Full pre-flight legality check of `LINK(source -> target)` against the
/// observation (used by every bot before emitting a LINK so that drops stay
/// near zero): source/target exist, distinct, both owned by `own`, the
/// agent is inside the closed operation range of the source, holds at least
/// one key of the target, the pair distance is within the closed
/// `MAX_LINK_DISTANCE`, no link exists on the canonical pair, no third
/// portal lies on the closed segment, and no existing link crosses the
/// segment without a shared endpoint (D38/D39 rules 1-11).
pub fn link_is_legal(
    obs: &Observation,
    own: Owner,
    cfg: &GameConfig,
    from: Coord,
    keys: &std::collections::BTreeMap<PortalId, u32>,
    source: PortalId,
    target: PortalId,
) -> bool {
    let Some(s) = obs.portals.iter().find(|p| p.id == source) else {
        return false;
    };
    let Some(t) = obs.portals.iter().find(|p| p.id == target) else {
        return false;
    };
    if source == target || s.owner != own || t.owner != own {
        return false;
    }
    if !in_range(from, s.position, cfg.portal_operation_range_cm) {
        return false;
    }
    if keys.get(&target).copied().unwrap_or(0) < 1 {
        return false;
    }
    let max2 = (cfg.max_link_distance_cm as i64) * (cfg.max_link_distance_cm as i64);
    if dist2(s.position, t.position) > max2 {
        return false;
    }
    let Some(link_id) = arena_core::types::LinkId::new(source, target) else {
        return false;
    };
    if obs.links.iter().any(|l| l.id == link_id) {
        return false;
    }
    // No third portal on the closed segment (endpoints exempt).
    if obs.portals.iter().any(|p| {
        p.id != source && p.id != target && point_on_segment(p.position, s.position, t.position)
    }) {
        return false;
    }
    // No illegal intersection with an existing link (shared endpoints are
    // exempt, D39).
    !obs.links.iter().any(|l| {
        let (Some(u), Some(v)) = (
            obs.portals.iter().find(|p| p.id == l.id.min),
            obs.portals.iter().find(|p| p.id == l.id.max),
        ) else {
            return false;
        };
        let shared = l.id.contains(source) || l.id.contains(target);
        !shared && segments_intersect(s.position, t.position, u.position, v.position)
    })
}
