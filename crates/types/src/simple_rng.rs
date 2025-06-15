#[derive(Debug)]
/// Simple PRNG compatible with JavaScript implementation
pub struct SimpleRNG {
    /// Current state of the generator
    seed: u32,
}

impl SimpleRNG {
    /// Creates new PRNG with given seed
    pub fn new(seed: u32) -> Self {
        Self { seed }
    }

    /// Generates next pseudorandom number
    #[expect(
        clippy::should_implement_trait,
        reason = "We do implement Iterator for SimpleRNG"
    )]
    pub fn next(&mut self) -> u32 {
        // Xorshift algorithm
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed = x;
        x
    }

    /// Generates random number between 0 and max (exclusive)
    /// NOTE: max values that approach u32::MAX will take a lot of time, due to the bias logic used!
    pub fn next_range(&mut self, max: u32) -> u32 {
        // prevent clamping/modulo bias
        let threshold = u32::MAX - (u32::MAX % max);
        let mut rand = self.next();
        // Keep generating new values until we get one below the threshold
        while rand >= threshold {
            rand = self.next(); // Get a new random value
        }
        // Now clamp it
        rand % max
    }
}

impl Iterator for SimpleRNG {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next())
    }
}
