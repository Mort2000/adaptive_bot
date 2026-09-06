//! Usage: adaptive-bot <server-socket> <seed>.
#![forbid(unsafe_code)]
use adaptive_bot::Adaptive;
use arena_client_sdk::{ConnectOptions, RunSummary, run};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "Usage: adaptive-bot <server-socket> <seed>\nSeed: unsigned decimal integer, supplied by the match runner."
        );
        return ExitCode::SUCCESS;
    }
    if args.len() != 2 {
        eprintln!("Usage: adaptive-bot <server-socket> <seed>");
        return ExitCode::from(2);
    }
    let Ok(seed) = args[1].parse::<u64>() else {
        eprintln!("seed must be an unsigned decimal integer");
        return ExitCode::from(2);
    };
    let options = ConnectOptions::new(&args[0], "adaptive-bot");
    match run(
        |faction, config, _map| Adaptive::new(faction, seed, config),
        options,
    ) {
        Ok(RunSummary::Normal {
            outcome,
            rounds_completed,
            score_enl,
            score_res,
        }) => {
            println!("outcome={outcome} rounds={rounds_completed} enl={score_enl} res={score_res}");
            ExitCode::SUCCESS
        }
        Ok(RunSummary::Aborted { outcome, detail }) => {
            eprintln!("aborted outcome={outcome} detail={detail:?}");
            ExitCode::from(3)
        }
        Err(error) => {
            eprintln!("client: {error}");
            ExitCode::from(2)
        }
    }
}
