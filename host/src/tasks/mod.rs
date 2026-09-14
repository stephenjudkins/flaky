//! Task implementations: fetching (binary cache NARs, `builtin:fetchurl`
//! derivations, flake source inputs), NAR hash verification, guest nix
//! evaluation, and building derivations in microVMs.

mod build;
pub(crate) mod eval;
mod fetch;
mod verify;

pub use build::BuildDrv;
pub use eval::{EvalFlake, EvalOutcome};
pub use fetch::{FetchNar, Fetchurl};
pub use verify::VerifyNarHash;
