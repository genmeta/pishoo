use tracing_subscriber::{fmt::writer::MakeWriterExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    let subscriber = dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User)
        .map_err(std::io::Error::other)
        .and_then(|home| logging(&home));
    match subscriber {
        Ok(subscriber) => subscriber.init(),
        Err(error) => {
            eprintln!("pishoo: cannot initialize error.log: {error}");
            std::process::exit(1);
        }
    }
    tracing::trace!(pid = std::process::id(), "Pishoo tracing initialized");
    if let Err(error) = pishoo::run().await {
        tracing::error!(%error, "Pishoo stopped");
        std::process::exit(1);
    }
}

fn logging(
    home: &dhttp_home::DhttpHome,
) -> std::io::Result<impl tracing::Subscriber + Send + Sync + use<>> {
    let directory = home.join("logs");
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(&directory)?;
    let path = directory.join("error.log");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(|error| {
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    })?;
    // The existing subscriber serializes each event through the owned file.
    // Keep stderr available to the service manager and interactive runs.
    let writer = std::sync::Mutex::new(file).and(std::io::stderr);
    Ok(tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(writer)
        .finish())
}
