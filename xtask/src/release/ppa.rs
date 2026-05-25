use snafu::Whatever;

pub async fn stage(_options: crate::release::PpaOptions) -> Result<(), Whatever> {
    snafu::whatever!("release subcommand not implemented yet")
}
