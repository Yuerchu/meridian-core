/// Which anchor a skill binding hangs off. Bindings are layered rather than
/// collected into one central set: a skill pinned globally stays available even
/// on an assistant the user cannot (or does not want to) edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "snake_case")]
pub enum SkillLayer {
    Global,
    Project,
    Assistant,
}

impl SkillLayer {
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}
