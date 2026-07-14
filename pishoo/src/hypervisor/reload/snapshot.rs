//! Root reload configuration phase.

pub async fn load_root_reload_snapshot(
    source: &crate::config::PishooConfigSource,
) -> Result<crate::config::GlobalPishooPlan, crate::config::plan::LoadGlobalPishooPlanError> {
    crate::config::load_global_pishoo_plan(source).await
}
