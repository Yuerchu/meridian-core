use super::*;
use serde_json::{Value, json};

fn fixture() -> Value {
    json!({
        "Id": "a".repeat(64), "Name": "/meridian-a-b", "Image": format!("sha256:{}", "b".repeat(64)),
        "Config": {"Image": "alpine:3.20", "Labels": { (OWNER_LABEL): OWNER_LABEL_VALUE,
            (SESSION_LABEL): "a/b", (SOURCE_LABEL): "/work" }},
        "Mounts": [{"Type":"bind", "Source":"/work", "Destination":"/workspace", "RW":true}],
        "State": {"Running":false, "Status":"exited"}
    })
}

fn accepts(value: Value) -> bool {
    let row: Inspect = serde_json::from_value(value).unwrap();
    row.verify(
        "a/b",
        Some("/work"),
        "alpine:3.20",
        &format!("sha256:{}", "b".repeat(64)),
    )
    .is_ok()
}

#[test]
fn reuse_requires_matching_identity_and_exactly_one_expected_mount() {
    assert!(accepts(fixture()));
    for (path, value) in [
        ("/Id", json!("abc")),
        ("/Name", json!("/other")),
        ("/Image", json!(format!("sha256:{}", "c".repeat(64)))),
        ("/Config/Image", json!("other")),
        ("/Mounts/0/Source", json!("/other")),
        ("/Mounts/0/Destination", json!("/other")),
        ("/Mounts/0/RW", json!(false)),
        ("/Mounts/0/Type", json!("volume")),
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = value;
        assert!(!accepts(v), "accepted altered {path}");
    }
    for (label, wrong) in [(OWNER_LABEL, "old"), (SESSION_LABEL, "a-b"), (SOURCE_LABEL, "/other")] {
        let mut v = fixture();
        v["Config"]["Labels"][label] = json!(wrong);
        assert!(!accepts(v), "accepted altered {label}");
    }
    let mut v = fixture();
    v["Mounts"].as_array_mut().unwrap().push(fixture()["Mounts"][0].clone());
    assert!(!accepts(v));
}

#[test]
fn stop_requires_a_stable_nonrunning_state() {
    for (running, status, expected) in [
        (false, "exited", true),
        (false, "created", true),
        (true, "exited", false),
        (false, "restarting", false),
    ] {
        let mut v = fixture();
        v["State"] = json!({"Running":running,"Status":status});
        assert_eq!(serde_json::from_value::<Inspect>(v).unwrap().stopped(), expected);
    }
}
