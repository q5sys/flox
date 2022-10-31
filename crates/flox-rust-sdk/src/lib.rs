extern crate pretty_env_logger;
#[macro_use]
extern crate log;
#[macro_use]
extern crate anyhow;

pub mod config;
pub mod providers;
pub mod utils;

mod models;

pub mod environment;

pub mod prelude {
    pub use crate::flox::Flox;
    pub use crate::models::catalog::Stability;
    pub use crate::nix::installable::Installable;
}

pub mod actions;
pub mod flox;
pub mod nix;
