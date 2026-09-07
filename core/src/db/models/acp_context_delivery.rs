use crate::db::schema::acp_context_deliveries;
use diesel::prelude::*;

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = acp_context_deliveries)]
pub struct AcpContextDeliveryRow {
    pub context_item_id: String,
    pub delivered_at: i64,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = acp_context_deliveries)]
pub struct AcpContextDeliveryInsert<'a> {
    pub context_item_id: &'a str,
    pub delivered_at: i64,
}
