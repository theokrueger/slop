use crate::config::{Config, Model};
use anyhow::{Context, Result, bail};
use std::io::{self, Write};

pub fn add_user(config: &mut Config, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("user name cannot be empty");
    }
    if config.users.contains_key(name) {
        bail!("user {name:?} already exists");
    }
    let key = format!("sk-{}", hex::encode(rand::random::<[u8; 32]>()));
    config.users.insert(name.into(), key.clone());
    Ok(key)
}

pub fn add_model(
    config: &mut Config,
    alias: &str,
    endpoint: &str,
    modelname: &str,
    api_key: &str,
) -> Result<()> {
    let alias = alias.trim();
    let endpoint = endpoint.trim().trim_end_matches('/');
    if alias.is_empty() {
        bail!("model alias cannot be empty");
    }
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        bail!("endpoint must start with http:// or https://");
    }
    if config.models.contains_key(alias) {
        bail!("model {alias:?} already exists");
    }
    let optional = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    config.models.insert(
        alias.into(),
        Model {
            endpoint: endpoint.into(),
            modelname: optional(modelname),
            api_key: optional(api_key),
            ..Default::default()
        },
    );
    Ok(())
}

pub fn prompt_model(config: &mut Config) -> Result<()> {
    let alias = ask("Model alias", None)?;
    let endpoint = ask("Endpoint", None)?;
    let modelname = ask("Upstream model name", Some(&alias))?;
    let api_key = ask("API key (optional)", None)?;
    add_model(config, &alias, &endpoint, &modelname, &api_key)?;
    println!("added model {alias}");
    Ok(())
}

fn ask(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(value) if !value.is_empty() => print!("{label} [{value}]: "),
        _ => print!("{label}: "),
    }
    io::stdout().flush().context("flush prompt")?;
    let mut line = String::new();
    io::stdin().read_line(&mut line).context("read input")?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.unwrap_or_default().to_string()
    } else {
        line.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_user_with_generated_key() {
        let mut config = Config::default();
        let key = add_user(&mut config, "me").unwrap();
        assert!(key.starts_with("sk-"));
        assert_eq!(config.users["me"], key);
        assert!(add_user(&mut config, "me").is_err());
    }

    #[test]
    fn adds_model_with_defaults() {
        let mut config = Config::default();
        add_model(&mut config, " alias ", "http://localhost/v1/", "", "").unwrap();
        let model = &config.models["alias"];
        assert_eq!(model.endpoint, "http://localhost/v1");
        assert!(model.modelname.is_none());
        assert!(model.api_key.is_none());
        assert!(add_model(&mut config, "alias", "http://x/v1", "", "").is_err());
        assert!(add_model(&mut config, "bad", "localhost", "", "").is_err());
    }
}
