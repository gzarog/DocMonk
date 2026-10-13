//! RagMonk configuration.
//!
//! * [`value`]: the ordered value tree a YAML document loads into;
//! * [`yaml`]: YAML 1.2 parsing and emission of that tree;
//! * [`coerce`]: conversion of loaded values (and environment strings) to
//!   typed fields, with plain error messages;
//! * [`model`]: the typed schema, defaults and validators;
//! * [`loader`]: layered loading (defaults < user < project < env < CLI),
//!   `config get/set` and `write_user_config`.
//!
//! Unknown keys are errors: every accepted setting has an effect.

pub mod coerce;
pub mod loader;
pub mod model;
pub mod value;
pub mod yaml;

pub use loader::{load_config, write_user_config, LoadOptions};
pub use model::RagMonkConfig;
pub use value::Value;
