//! Rewrite `model_profiles.capability_overrides` from `supports_images` /
//! `supports_pdf` booleans into `readable_mime_types`.
//!
//! Data only: the column is still JSON text. Old keys are unknown fields after
//! this change, so every stored patch has to move in the same revision.

pub fn sqlite_statements() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn rewritten_patches_parse_as_the_new_contract() {
        let rewritten = crate::provider::capabilities::rewrite_legacy_media_overrides(r#"{"supports_images":false}"#)
            .unwrap()
            .unwrap();
        crate::provider::capabilities::ProviderCapabilityOverrides::parse(&rewritten).unwrap();
        assert!(
            crate::provider::capabilities::rewrite_legacy_media_overrides(r#"{"supports_fast":true}"#)
                .unwrap()
                .is_none()
        );
    }
}
