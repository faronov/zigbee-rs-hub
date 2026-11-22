//! Host-test support: reference expression interpreter, YAML loader of
//! the definition examples, a device simulator (also with the `sim`
//! feature: `hubctl --sim-devices`).

#[cfg(test)]
pub mod expr;
pub mod sim;
#[cfg(test)]
pub mod yaml;
