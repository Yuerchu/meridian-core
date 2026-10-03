// Outside `crate::db` there is no way to reach the connection behind a capability.
use meridian_core::db::sea::cap::sealed::Access;
use meridian_core::db::sea::cap::Db;

fn caller(db: &Db) {
    let _ = db.conn();
}

fn main() {}
