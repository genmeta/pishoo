use std::path::Path;

use pishoo::{DaemonConfig, Error};
type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() {
    let result = async {
        let home = dhttp_home::DhttpHome::load(dhttp_home::HomeScope::User)
            .map_err(|e| pishoo::Error::InvalidConfig(e.to_string()))?;
        let path = home.join("pishoo.toml");
        let config = load_config(&path)?;
        pishoo::run(config).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("pishoo: {error}");
        std::process::exit(1);
    }
}

fn load_config(path: &Path) -> Result<DaemonConfig> {
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|e| Error::InvalidConfig(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instance_config_rejects_unknown_fields_and_defaults_terminal_off() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pishoo.toml");
        std::fs::write(&path, "state_dir='/tmp/example'\n").unwrap();
        assert!(!load_config(&path).unwrap().terminal.enabled);
        std::fs::write(&path, "state_dir='/tmp/example'\nrequest_limit=4\n").unwrap();
        assert!(load_config(&path).is_err());
        std::fs::write(
            &path,
            "state_dir='/tmp/example'\n[terminal]\nenabled=false\nbackend='native'\n",
        )
        .unwrap();
        assert!(load_config(&path).is_err());
    }
}
