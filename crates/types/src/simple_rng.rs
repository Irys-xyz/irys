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
    pub fn next_range(&mut self, max: u32) -> u32 {
        self.next() % max
    }
}
