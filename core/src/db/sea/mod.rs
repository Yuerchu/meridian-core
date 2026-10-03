//! The SeaORM side of the persistence layer, alongside Diesel while modules
//! move over one transaction root at a time.

pub mod cap;

#[cfg(test)]
mod poc;
