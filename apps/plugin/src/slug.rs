//! Three-word room names: `<adjective>-<adjective>-<noun>`.

const ADJECTIVES: &[&str] = &[
    "big", "filthy", "quiet", "late", "warm", "cold", "loud", "soft", "bright", "dark", "wild",
    "rusty", "dusty", "sweet", "heavy", "light", "sharp", "empty", "slow", "fast", "deep", "thin",
    "wide", "tiny", "pale", "dry", "calm", "raw", "odd", "bold", "hot", "cool", "flat", "polar",
    "lunar", "storm", "still", "vivid", "mute", "gold",
];

const NOUNS: &[&str] = &[
    "papaya", "mango", "cedar", "maple", "river", "stone", "fox", "wolf", "moth", "ember", "comet",
    "harbor", "attic", "kettle", "drum", "piano", "socket", "buffer", "fader", "meter", "booth",
    "desk", "lamp", "tape", "reel", "stem", "gate", "plate", "spring", "orchid", "canyon",
    "glacier", "meadow", "velvet", "copper", "quartz",
];

/// A fresh, human-readable slug. Seeded from wall clock and pid, mixed
/// with splitmix64; uniqueness is per instance, not cryptographic.
pub fn new_slug() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |elapsed| elapsed.as_nanos() as u64);
    let pid = u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    from_seed(nanos ^ pid)
}

fn from_seed(seed: u64) -> String {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;

    let n_adj = ADJECTIVES.len() as u64;
    let n_noun = NOUNS.len() as u64;
    let first = (z % n_adj) as usize;
    let mut second = ((z / n_adj) % n_adj) as usize;
    let noun = ((z / n_adj / n_adj) % n_noun) as usize;
    if second == first {
        second = (second + 1) % ADJECTIVES.len();
    }
    format!(
        "{}-{}-{}",
        ADJECTIVES[first], ADJECTIVES[second], NOUNS[noun]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_three_distinct_known_words() {
        for seed in [0, 1, 7, 42, u64::MAX, 0xDEAD_BEEF] {
            let slug = from_seed(seed);
            let parts: Vec<_> = slug.split('-').collect();
            assert_eq!(parts.len(), 3, "{slug}");
            assert!(ADJECTIVES.contains(&parts[0]), "{slug}");
            assert!(ADJECTIVES.contains(&parts[1]), "{slug}");
            assert!(NOUNS.contains(&parts[2]), "{slug}");
            assert_ne!(parts[0], parts[1], "{slug}");
        }
    }

    #[test]
    fn slug_survives_normalization_unchanged() {
        let slug = new_slug();
        assert_eq!(relay_session::normalize_slug(&slug), slug);
    }
}
