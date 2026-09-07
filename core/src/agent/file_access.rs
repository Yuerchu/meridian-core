use crate::db::DbPool;
use crate::tools;

/// Persisted SAF root entry, stored as a JSON array under the
/// "android.saf_roots" preference key.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafRootEntry {
    pub uri: String,
    pub display_name: String,
    pub virtual_prefix: String,
}

#[cfg(any(target_os = "android", test))]
fn parse_saf_roots(value: Option<&str>) -> Result<Vec<SafRootEntry>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(json) => serde_json::from_str(json).map_err(|error| format!("invalid `android.saf_roots`: {error}")),
    }
}

/// Build the file access policy for tool execution.
/// Desktop: unrestricted (legacy working_directory validation only).
/// Android: whitelist of authorized roots from preferences + system grants.
pub async fn build_file_access(pool: &DbPool) -> Result<tools::FileAccess, String> {
    #[cfg(target_os = "android")]
    {
        let pool = pool.clone();
        let (manage_pref, saf_pref) = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let mut conn = pool.get().map_err(|error| error.to_string())?;
            let manage = crate::db::ops::preference::get_preference(&mut conn, "android.manage_storage_enabled")
                .map_err(|error| error.to_string())?;
            let saf = crate::db::ops::preference::get_preference(&mut conn, "android.saf_roots")
                .map_err(|error| error.to_string())?;
            Ok((manage, saf))
        })
        .await
        .map_err(|error| error.to_string())??;
        let manage_enabled = crate::db::ops::preference::parse_bool_preference(
            "android.manage_storage_enabled",
            manage_pref.as_deref(),
            false,
        )?;
        let saf_entries = parse_saf_roots(saf_pref.as_deref())?;

        let mut roots = Vec::new();
        if manage_enabled && crate::android_bridge::is_manage_storage_granted()? {
            let shared = std::path::PathBuf::from("/storage/emulated/0");
            roots.push(tools::AccessRoot {
                virtual_prefix: "/storage/emulated/0".to_string(),
                kind: tools::RootKind::RealPath(shared.clone()),
            });
            roots.push(tools::AccessRoot {
                virtual_prefix: "/sdcard".to_string(),
                kind: tools::RootKind::RealPath(shared),
            });
            if let Ok(rd) = std::fs::read_dir("/storage") {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name == "emulated" || name == "self" {
                        continue;
                    }
                    let p = entry.path();
                    if p.is_dir() {
                        roots.push(tools::AccessRoot {
                            virtual_prefix: format!("/storage/{name}"),
                            kind: tools::RootKind::RealPath(p),
                        });
                    }
                }
            }
        }
        if !saf_entries.is_empty() {
            let valid_uris: std::collections::HashSet<String> =
                tokio::task::spawn_blocking(crate::android_bridge::persisted_tree_uris)
                    .await
                    .map_err(|error| error.to_string())??
                    .into_iter()
                    .collect();
            for entry in saf_entries {
                if valid_uris.contains(&entry.uri) {
                    roots.push(tools::AccessRoot {
                        virtual_prefix: entry.virtual_prefix,
                        kind: tools::RootKind::SafTree { tree_uri: entry.uri },
                    });
                }
            }
        }
        Ok(tools::FileAccess::Roots(roots))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = pool;
        Ok(tools::FileAccess::default())
    }
}

/// Describe accessible file roots for the system prompt so the model knows
/// what paths it may use. Empty string when not in roots mode.
pub fn file_access_prompt(file_access: &tools::FileAccess) -> String {
    let tools::FileAccess::Roots(roots) = file_access else {
        return String::new();
    };
    if roots.is_empty() {
        return "\n\n# File access\nNo file locations are currently authorized on this device. \
                If the user asks for file operations, tell them to grant access in \
                Settings (an authorized directory or 'All files access')."
            .to_string();
    }
    let mut out = String::from("\n\n# File access\nYou can access files under these locations (use absolute paths):\n");
    for root in roots {
        match &root.kind {
            tools::RootKind::RealPath(_) => {
                out.push_str(&format!("- {} (direct access)\n", root.virtual_prefix));
            }
            tools::RootKind::SafTree { .. } => {
                out.push_str(&format!(
                    "- {} (user-authorized directory; recursive search/glob unavailable)\n",
                    root.virtual_prefix
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_saf_roots;

    #[test]
    fn saf_roots_require_the_canonical_array_shape() {
        assert!(parse_saf_roots(None).unwrap().is_empty());
        assert!(parse_saf_roots(Some("[]")).unwrap().is_empty());
        assert!(parse_saf_roots(Some("{}")).is_err());
        assert!(
            parse_saf_roots(Some(
                r#"[{"uri":"u","display_name":"d","virtual_prefix":"/v","future":true}]"#
            ))
            .is_err()
        );
        assert!(parse_saf_roots(Some(r#"[{"uri":"u","display_name":"d"}]"#)).is_err());
    }
}
