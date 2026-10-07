//! `skill_bindings_assistant`: skills offered wherever one assistant answers.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "skill_bindings_assistant")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub assistant_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub dir_name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::skill::Entity",
        from = "Column::DirName",
        to = "super::skill::Column::DirName",
        on_delete = "Cascade"
    )]
    Skill,
    #[sea_orm(
        belongs_to = "super::assistant::Entity",
        from = "Column::AssistantId",
        to = "super::assistant::Column::Id",
        on_delete = "Cascade"
    )]
    Assistant,
}

impl Related<super::skill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Skill.def()
    }
}

impl Related<super::assistant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Assistant.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
