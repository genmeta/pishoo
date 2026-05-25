use snafu::Whatever;

pub async fn stage() -> Result<(), Whatever> {
    snafu::whatever!("release subcommand not implemented yet")
}
