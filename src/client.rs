//! The official SDK owns transport, observation access, and round deadlines.
use crate::Adaptive;
use arena_client_sdk::{ClientBot, RoundContext, SdkError, SpawnContext};
use arena_core::types::{AgentId, Coord};

impl ClientBot for Adaptive {
    fn choose_spawns(&mut self, ctx: &SpawnContext<'_>) -> Vec<(AgentId, Coord)> {
        let map = ctx
            .map_view()
            .expect("spawn map is readable")
            .to_map_definition()
            .expect("valid server map");
        self.choose_spawns(&map, &ctx.roster())
    }

    fn on_round(&mut self, mut ctx: RoundContext<'_>) {
        let observation = match ctx.observation().and_then(|view| view.to_observation()) {
            Ok(observation) => observation,
            Err(error) => {
                eprintln!("observation: {error}");
                return;
            }
        };
        for submission in self.submit_round(&observation) {
            if let Err(error) = ctx.submit(submission.agent_id, submission.commands) {
                eprintln!("submit: {error}");
                return;
            }
        }
        // The last agent's submission may already have closed the context.
        if let Err(error) = ctx.commit() {
            if !matches!(error, SdkError::ContextClosed) {
                eprintln!("commit: {error}");
            }
        }
    }
}
