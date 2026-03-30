pub mod conflict;
pub mod scorer;
pub mod state_machine;

pub use conflict::resolve_conflicts;
pub use scorer::{ConfluenceScore, Direction};
pub use state_machine::{SignalMachine, SignalState};
