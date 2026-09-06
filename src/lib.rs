//! Alpha reference player. Start with `main.rs`, `client.rs`, then `adaptive.rs`.
#![forbid(unsafe_code)]

mod adaptive;
mod attack;
mod build;
mod client;
mod common;
mod compound;
mod defense;
mod opponent;
mod rng;
mod sample;
mod tasks;
mod value;

pub use adaptive::Adaptive;
