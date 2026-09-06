//! Deterministic, faction-specific tie breaks. The key preserves r2.6 Adaptive's stream.
use arena_core::rng::uniform_from_words;
use arena_core::types::Faction;

#[derive(Debug)]
pub struct BotRng {
    reader: blake3::OutputReader,
}

impl BotRng {
    pub fn new(seed: u64, faction: Faction) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ABOT");
        hasher.update(&1u32.to_le_bytes());
        hasher.update(&seed.to_le_bytes());
        hasher.update(&[
            match faction {
                Faction::Enl => 0,
                Faction::Res => 1,
            },
            11,
        ]);
        Self {
            reader: hasher.finalize_xof(),
        }
    }

    pub fn uniform(&mut self, upper: u32) -> u32 {
        uniform_from_words(&mut self.reader, upper)
    }
}
