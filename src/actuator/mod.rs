//! The actuator: turns controller decisions into input driven into the game
//! window. `plan` is the pure half (zones, transform, timed job builders);
//! the executor and the Windows input backend arrive with the wiring.

pub mod plan;
