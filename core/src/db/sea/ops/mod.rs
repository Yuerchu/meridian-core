//! The SeaORM ops, one module per table, filled in as transaction roots move
//! over. A module here that shares a name with one in `db/ops` is a dual
//! implementation while Diesel callers remain; `docs/dual-impl.md` lists each
//! such pair with its remaining Diesel callers, and the checker keeps the two
//! in step.

pub mod preference;
