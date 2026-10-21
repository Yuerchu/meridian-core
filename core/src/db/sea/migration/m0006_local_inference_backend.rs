//! 本地推理后端是供应商设置，不是从文件名猜出的模型属性。

pub fn sqlite_statements() -> Vec<String> {
    // backend: sqlite-only — ADD COLUMN CHECK 和定向数据回填不重建被引用的 providers 表。
    vec![
        "ALTER TABLE providers ADD COLUMN local_backend TEXT CHECK (local_backend IN ('auto', 'cpu', 'gpu', 'npu'))"
            .into(),
        "UPDATE providers SET local_backend = 'auto' WHERE provider_type = 'litert_lm'".into(),
        "DELETE FROM cached_models WHERE provider_id IN (SELECT id FROM providers WHERE provider_type = 'litert_lm')"
            .into(),
    ]
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database};

    #[tokio::test]
    async fn migration_backfills_only_local_providers_and_only_clears_their_cache() {
        let database_url = "sqlite::memory:";
        assert_eq!(database_url, "sqlite::memory:");
        let db = Database::connect(database_url).await.unwrap();
        for sql in super::super::m0001_baseline::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        for sql in [
            "INSERT INTO providers (id,name,provider_type,base_url,created_at,updated_at) VALUES ('local','Local','litert_lm','C:/model.litertlm',1,1)",
            "INSERT INTO providers (id,name,provider_type,base_url,created_at,updated_at) VALUES ('cloud','Cloud','openai','https://example.invalid',1,1)",
            "INSERT INTO cached_models (provider_id,model_id,model_name,fetched_at) VALUES ('local','gemma-4-E4B-it','old static list',1),('cloud','cloud-model','Cloud',1)",
            "INSERT INTO model_profiles (id,name,context_window,compact_threshold,max_output_tokens,created_at,updated_at) VALUES ('profile','User alias',8192,4096,1024,1,1)",
            "INSERT INTO model_configs (id,provider_id,model_id,profile_id,created_at,updated_at) VALUES ('config','local','user-existing-alias','profile',1,1)",
        ] {
            db.execute_unprepared(sql).await.unwrap();
        }
        for sql in super::sqlite_statements() {
            db.execute_unprepared(&sql).await.unwrap();
        }
        let rows = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT id, local_backend FROM providers ORDER BY id",
            ))
            .await
            .unwrap();
        assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "cloud");
        assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap(), None);
        assert_eq!(rows[1].try_get_by_index::<String>(1).unwrap(), "auto");
        let cache = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT provider_id FROM cached_models",
            ))
            .await
            .unwrap();
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].try_get_by_index::<String>(0).unwrap(), "cloud");
        let config = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT model_id FROM model_configs WHERE id='config'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config.try_get_by_index::<String>(0).unwrap(), "user-existing-alias");
        assert!(
            db.execute_unprepared("UPDATE providers SET local_backend='future' WHERE id='local'")
                .await
                .is_err()
        );
    }
}
