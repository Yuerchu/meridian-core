//! What `db::sea::cap` promises at compile time, kept honest: each case here
//! is a way to write that must not build.

#[test]
fn capabilities_refuse_writes_they_do_not_carry() {
    trybuild::TestCases::new().compile_fail("tests/ui/cap/*.rs");
}
