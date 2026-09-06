//! Sample among near-equal candidate values using the match-local random stream.

use crate::rng::BotRng;

/// Seeded near-equal sampling: the best candidate plus every candidate
/// whose value is within `near_equal_permille` permille of the best
/// (integer truncation: `best * permille / 1000`, floor = best - slack)
/// form the eligible band, and one member is drawn uniformly through the
/// stream. Rules:
///
/// - `near_equal_permille == 0` keeps only exact ties with the best;
/// - a best value of zero or below has no proportional band (only exact
///   ties) — negative values never widen the band;
/// - deterministic given the stream position: same key and same call
///   sequence, same pick; seed and faction key the stream.
pub fn pick_near_equal<'a, T, F>(
    items: &'a [T],
    value: F,
    near_equal_permille: u32,
    rng: &mut BotRng,
) -> Option<&'a T>
where
    F: Fn(&T) -> i64,
{
    if items.is_empty() {
        return None;
    }
    let best = items.iter().map(|item| value(item)).max()?;
    let slack = if best > 0 {
        ((best as u128 * near_equal_permille as u128) / 1000).min(i64::MAX as u128) as i64
    } else {
        0
    };
    let floor = best.saturating_sub(slack);
    let eligible: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| value(item) >= floor)
        .map(|(index, _)| index)
        .collect();
    let index = eligible[rng.uniform(eligible.len() as u32) as usize];
    items.get(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_core::types::Faction;

    #[test]
    fn the_band_floor_is_truncated_integer_permille() {
        // best 1000: slack = best * permille / 1000 truncated, floor =
        // best - slack. permille 33 -> slack 33 -> floor 967, so 968 STAYS
        // in the band and the uniform draw may legitimately return either
        // member; permille 31 -> slack 31 -> floor 969, which excludes 968
        // on every stream.
        let mut rng = BotRng::new(1, Faction::Enl);
        let pick = pick_near_equal(&[1000i64, 968], |v| *v, 33, &mut rng).map(|v| *v);
        assert!(
            pick == Some(1000) || pick == Some(968),
            "floor 967 keeps 968 eligible, got {pick:?}"
        );
        for seed in 0..32u64 {
            let mut rng = BotRng::new(seed, Faction::Enl);
            assert_eq!(
                pick_near_equal(&[1000i64, 968], |v| *v, 31, &mut rng).map(|v| *v),
                Some(1000),
                "floor 969 excludes 968 on every stream (seed {seed})"
            );
        }
        let mut rng = BotRng::new(1, Faction::Enl);
        assert!(
            pick_near_equal(&[1000i64, 967, 900], |v| *v, 33, &mut rng).map(|v| *v) != Some(900),
            "900 is outside the band"
        );
        // Every draw over a fixed stream is stable across 64 repetitions.
        let mut rng = BotRng::new(9, Faction::Res);
        let picks: Vec<i64> = (0..64)
            .map(|_| *pick_near_equal(&[100i64, 99, 90], |v| *v, 10, &mut rng).unwrap())
            .collect();
        assert!(picks.iter().all(|v| *v >= 99));
    }
}
