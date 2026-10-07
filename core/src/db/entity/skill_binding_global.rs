//! `skill_bindings_global`: skills offered in every conversation.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "skill_bindings_global")]
pub struct Model {
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
}

impl Related<super::skill::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Skill.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
