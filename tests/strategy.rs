//! Tests use the same public strategy interface as the SDK adapter.
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use adaptive_bot::Adaptive;
use arena_core::config::GameConfig;
use arena_core::event::{Event, EventKind};
use arena_core::harness::{AgentSubmission, MatchHarness, RoundStrategy, ScriptedStrategy};
use arena_core::map::generate_map;
use arena_core::observe::{AgentPrivate, FieldPublic, LinkPublic, Observation, PortalPublic};
use arena_core::spawn::validate_initial_spawn_set;
use arena_core::state::GameState;
use arena_core::types::{AgentId, Command, Coord, Faction, FieldId, LinkId, Owner, PortalId};

struct Compared {
    player: Adaptive,
    twin: Adaptive,
    active: Rc<RefCell<BTreeSet<AgentId>>>,
    command_limit: usize,
}

impl RoundStrategy for Compared {
    fn submit_round(&mut self, observation: &Observation) -> Vec<AgentSubmission> {
        let submissions = self.player.submit_round(observation);
        assert_eq!(submissions, self.twin.submit_round(observation));
        let mut submitted = BTreeSet::new();
        for submission in &submissions {
            assert!(
                submitted.insert(submission.agent_id),
                "one stream per agent"
            );
            assert!(
                observation
                    .own_agents
                    .iter()
                    .any(|agent| agent.id == submission.agent_id)
            );
            assert!(submission.commands.len() <= self.command_limit);
            let major = submission
                .commands
                .iter()
                .filter(|command| {
                    matches!(
                        command,
                        Command::Attack | Command::Recharge { .. } | Command::Capture { .. }
                    )
                })
                .count();
            assert!(major <= 1, "compound streams share one major-action slot");
            if !submission.commands.is_empty() {
                self.active.borrow_mut().insert(submission.agent_id);
            }
        }
        submissions
    }
}

#[test]
fn builds_scores_and_uses_reinforcements_deterministically_on_both_sides() {
    for (recharge_amount, max_link_distance_cm) in [(20, 150_000), (15, 200_000)] {
        let config = GameConfig {
            total_rounds: 100,
            reinforcement_interval_rounds: 20,
            map_width_cm: 200_000,
            map_height_cm: 200_000,
            portal_count: 24,
            portal_cluster_count: 4,
            recharge_amount,
            max_link_distance_cm,
            ..GameConfig::default()
        };
        let map = generate_map(7, &config).unwrap();
        let (enl, res) = GameState::initial_roster(&config);
        for faction in [Faction::Enl, Faction::Res] {
            let roster = if faction == Faction::Enl { &enl } else { &res };
            let enemy_roster = if faction == Faction::Enl { &res } else { &enl };
            let mut player = Adaptive::new(faction, 42, &config);
            let mut twin = Adaptive::new(faction, 42, &config);
            let entries = player.choose_spawns(&map, roster);
            assert_eq!(entries, twin.choose_spawns(&map, roster));
            let ours = validate_initial_spawn_set(roster, &entries, &config).unwrap();
            let theirs = validate_initial_spawn_set(
                enemy_roster,
                &enemy_roster
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| (id, map.portal(PortalId(i as u32)).unwrap().position))
                    .collect::<Vec<_>>(),
                &config,
            )
            .unwrap();
            let active = Rc::new(RefCell::new(BTreeSet::new()));
            let subject = Box::new(Compared {
                player,
                twin,
                active: active.clone(),
                command_limit: config.max_commands_per_agent_round as usize,
            });
            let idle = Box::new(ScriptedStrategy::new(vec![
                vec![];
                config.total_rounds as usize
            ]));
            let mut game = if faction == Faction::Enl {
                MatchHarness::new(19, config, &map, &ours, &theirs, subject, idle)
            } else {
                MatchHarness::new(19, config, &map, &theirs, &ours, idle, subject)
            }
            .unwrap();
            for _ in 0..config.total_rounds {
                game.run_round();
            }
            assert!(
                game.state().score(faction) > 0,
                "construction produces actual income"
            );
            assert!(
                active.borrow().iter().any(|id| !roster.contains(id)),
                "reinforcements receive real commands"
            );
        }
    }
}

fn threatened_field() -> Observation {
    let portals = [(4_000, 4_000), (16_000, 4_000), (4_000, 16_000)]
        .into_iter()
        .enumerate()
        .map(|(i, (x, y))| PortalPublic {
            id: PortalId(i as u32),
            position: Coord::new(x, y),
            owner: Owner::Enl,
            hp: if i == 0 { 20 } else { 100 },
        })
        .collect();
    Observation {
        round: 101,
        portals,
        links: [(0, 1), (0, 2), (1, 2)]
            .into_iter()
            .map(|(a, b)| LinkPublic {
                id: LinkId::new(PortalId(a), PortalId(b)).unwrap(),
                faction: Faction::Enl,
            })
            .collect(),
        fields: vec![FieldPublic {
            id: FieldId::new(PortalId(0), PortalId(1), PortalId(2)).unwrap(),
            faction: Faction::Enl,
            area2: 144_000_000,
        }],
        events: vec![Event {
            seq: 0,
            round: 100,
            kind: EventKind::PortalAttacked {
                faction: Faction::Res,
                agent_id: AgentId(3),
                portal: PortalId(0),
                damage: 20,
                hp_after: 20,
            },
        }],
        scores: BTreeMap::from([(Faction::Enl, 0), (Faction::Res, 0)]),
        own_agents: vec![AgentPrivate {
            id: AgentId(0),
            position: Coord::new(16_000, 4_000),
            supply: 5,
            keys: BTreeMap::from([(PortalId(0), 1)]),
        }],
    }
}

#[test]
fn remote_maintenance_includes_replenishment() {
    let observation = threatened_field();
    let mut player = Adaptive::new(Faction::Enl, 42, &GameConfig::default());
    let streams = player.submit_round(&observation);
    let commands = &streams
        .iter()
        .find(|submission| submission.agent_id == AgentId(0))
        .unwrap()
        .commands;
    assert!(
        commands.contains(&Command::Recharge {
            portal: PortalId(0)
        }),
        "{commands:?}"
    );
    assert!(
        commands.contains(&Command::Hack {
            portal: PortalId(1)
        }),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, Command::Transfer { .. }))
    );
}

#[test]
fn attack_candidates_are_available_before_the_endgame() {
    let mut observation = threatened_field();
    observation.round = 2;
    observation.events.clear();
    for portal in &mut observation.portals {
        portal.owner = Owner::Res;
        portal.hp = 20;
    }
    for link in &mut observation.links {
        link.faction = Faction::Res;
    }
    observation.fields[0].faction = Faction::Res;
    observation.own_agents[0].position = observation.portals[0].position;
    let mut player = Adaptive::new(Faction::Enl, 42, &GameConfig::default());
    let streams = player.submit_round(&observation);
    assert!(
        streams
            .iter()
            .any(|submission| submission.commands.contains(&Command::Attack)),
        "{streams:?}"
    );
}
