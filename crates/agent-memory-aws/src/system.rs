//! Real implementations of the two "ambient" driven ports.
//!
//! They live beside the AWS adapters rather than in the domain so that every
//! port has exactly one home for its production implementation.

use std::time::SystemTime;

use agent_memory_core::{Clock, IdGenerator, MemoryId};

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Random v4 identifiers.
#[derive(Debug, Clone, Copy, Default)]
pub struct UuidGenerator;

impl IdGenerator for UuidGenerator {
    fn next_id(&self) -> MemoryId {
        // A hyphenated UUID is never empty, so the only failure mode of
        // `MemoryId::new` cannot occur here.
        MemoryId::new(uuid::Uuid::new_v4().to_string()).unwrap_or_else(|_| unreachable!())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identifiers_are_unique() {
        let generator = UuidGenerator;
        assert_ne!(generator.next_id(), generator.next_id());
    }
}
