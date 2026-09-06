//! Deterministic task allocation with shared reservations.
//! One-shot work has one assignee; coordinated attacks and rescues declare capacity.

use std::collections::BTreeMap;

use arena_core::types::{AgentId, PortalId};

/// One tactical task for one agent. Two tasks are the
/// same reservation when their variants and targets are equal. Link
/// directions share one undirected edge reservation; the grant retains
/// its original direction so execution consumes the correct target Key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Task {
    /// Take a neutral portal (Major Action).
    Capture { portal: PortalId },
    /// HACK a portal for keys and Supply (the key-acquisition task).
    KeyFarm { portal: PortalId },
    /// Create the link `source -> target` (consumes a Key of `target`).
    Link { source: PortalId, target: PortalId },
    /// Attack an enemy portal, optionally as a coordinated strike.
    Attack { portal: PortalId },
    /// Recharge an own anchor (Major Action, consumes Supply), optionally
    /// as an explicitly valued and revalidated coordinated rescue.
    Recharge { portal: PortalId },
}

impl Task {
    fn reservation(self) -> Self {
        match self {
            Self::Link { source, target } => Self::Link {
                source: source.min(target),
                target: source.max(target),
            },
            task => task,
        }
    }

    /// The portal this task occupies.
    pub fn target_portal(&self) -> PortalId {
        match self {
            Task::Capture { portal }
            | Task::KeyFarm { portal }
            | Task::Attack { portal }
            | Task::Recharge { portal } => *portal,
            Task::Link { source, .. } => *source,
        }
    }
}

/// One agent's ranked request for a task: `priority` orders requests (the
/// value model's integer scores slot in directly) and
/// `assignee_capacity` declares how many agents the task admits — `1` for
/// every one-shot task, `> 1` only for an explicit coordinated attack or
/// rescue; both require actual-cohort value checks in the builder core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskRequest {
    pub task: Task,
    pub priority: i64,
    pub assignee_capacity: u32,
}

impl TaskRequest {
    /// A one-shot request at `priority`.
    pub fn one_shot(task: Task, priority: i64) -> TaskRequest {
        TaskRequest {
            task,
            priority,
            assignee_capacity: 1,
        }
    }

    /// An explicitly coordinated combat request with its declared capacity.
    pub fn coordinated(task: Task, priority: i64, capacity: u32) -> TaskRequest {
        TaskRequest {
            task,
            priority,
            assignee_capacity: capacity.max(1),
        }
    }
}

/// One round's reservation ledger. Constructed fresh each round (claims
/// never leak across rounds); the board
/// itself never looks at agent positions or the observation, so it stays
/// a pure contention resolver.
#[derive(Clone, Debug)]
pub struct TaskBoard {
    remaining: BTreeMap<Task, u32>,
}

impl TaskBoard {
    /// An empty board for the current round.
    pub fn new() -> TaskBoard {
        TaskBoard {
            remaining: BTreeMap::new(),
        }
    }

    /// Tries to reserve one slot of `task`. The FIRST claim fixes the
    /// task's capacity for the round (`capacity_if_new`: 1 for one-shot
    /// tasks, the declared headcount for coordinated combat); later
    /// claims succeed only while capacity remains.
    pub fn claim(&mut self, task: Task, capacity_if_new: u32) -> bool {
        let task = task.reservation();
        let capacity = capacity_if_new.max(1);
        let remaining = self.remaining.entry(task).or_insert(capacity);
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        true
    }
}

/// Grants at most one task per agent, resolving all contention through
/// `board`: every `(agent, request)` pair is considered once, in
/// (priority descending, agent id ascending, task ascending) order —
/// highest-value work is granted first, equal priorities go to the
/// lower agent id, and an agent falls through to its next-ranked request
/// when its preferred task is exhausted. A pure deterministic function
/// of its inputs; agents not present in `requests` receive nothing.
pub fn allocate(
    board: &mut TaskBoard,
    agents: &[AgentId],
    requests: &BTreeMap<AgentId, Vec<TaskRequest>>,
) -> BTreeMap<AgentId, Task> {
    allocate_compatible(board, agents, requests, |_, _, _| true)
}

/// The observation-aware core can reject mutually incompatible grants
/// before reserving a task; rejected agents still try lower-ranked work.
pub(crate) fn allocate_compatible(
    board: &mut TaskBoard,
    agents: &[AgentId],
    requests: &BTreeMap<AgentId, Vec<TaskRequest>>,
    compatible: impl Fn(AgentId, Task, &BTreeMap<AgentId, Task>) -> bool,
) -> BTreeMap<AgentId, Task> {
    let mut flattened: Vec<(i64, AgentId, TaskRequest)> = Vec::new();
    for &agent in agents {
        if let Some(agent_requests) = requests.get(&agent) {
            for request in agent_requests {
                flattened.push((request.priority, agent, *request));
            }
        }
    }
    flattened.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.task.cmp(&b.2.task))
    });

    let mut granted: BTreeMap<AgentId, Task> = BTreeMap::new();
    for (_, agent, request) in flattened {
        if granted.contains_key(&agent) {
            continue; // one task per agent per round
        }
        if compatible(agent, request.task, &granted)
            && board.claim(request.task, request.assignee_capacity)
        {
            granted.insert(agent, request.task);
        }
    }
    granted
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit pins of the reservation semantics the integration fixtures
    // exercise through allocate().

    #[test]
    fn first_claim_fixes_capacity_even_if_declared_later() {
        let task = Task::Recharge {
            portal: PortalId(1),
        };
        let mut board = TaskBoard::new();
        // The first claim's capacity is the round's contract: a later,
        // larger declaration cannot widen a one-shot reservation.
        assert!(board.claim(task, 1));
        assert!(!board.claim(task, 3));
    }

    #[test]
    fn zero_capacity_clamps_to_one_shot() {
        let task = Task::KeyFarm {
            portal: PortalId(0),
        };
        let mut board = TaskBoard::new();
        assert!(board.claim(task, 0));
        assert!(!board.claim(task, 0));
    }

    #[test]
    fn opposite_links_share_one_reservation() {
        let mut board = TaskBoard::new();
        let forward = Task::Link {
            source: PortalId(1),
            target: PortalId(2),
        };
        let backward = Task::Link {
            source: PortalId(2),
            target: PortalId(1),
        };
        assert!(board.claim(forward, 1));
        assert!(!board.claim(backward, 1));
    }
}
