//! Environment overlays: `[environments.<name>]` is deep-merged over the
//! base. Tables merge key by key; anything else (strings, numbers, arrays)
//! replaces, so `domains = []` clears the base's list. A `source` naming its
//! kind (`repo`, `image` or `local`) replaces the base's source too, so an
//! image can't end up merged into a repository source; `source.branch = …`
//! alone still merges.

/// Merge `overlay` into `base`.
pub fn deep_merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        let new_source = key == "source"
            && value.as_table().is_some_and(|t| {
                ["repo", "image", "local"]
                    .iter()
                    .any(|k| t.contains_key(*k))
            });
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) if !new_source => deep_merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_merge_and_everything_else_replaces() {
        let mut base: toml::Table = toml::from_str(
            r#"
            [services.api]
            port = 80
            domains = ["a.example.com"]
            source = { repo = "acme/api", branch = "main" }
            variables = { A = "1", B = "2" }
            "#,
        )
        .unwrap();
        let overlay: toml::Table = toml::from_str(
            r#"
            services.api.domains = []
            services.api.source.branch = "dev"
            services.api.variables.B = { secret = true }
            services.web.port = 3000
            "#,
        )
        .unwrap();
        deep_merge(&mut base, overlay);
        let api = &base["services"]["api"];
        assert_eq!(api["port"].as_integer(), Some(80));
        assert_eq!(api["domains"].as_array().map(Vec::len), Some(0));
        assert_eq!(api["source"]["repo"].as_str(), Some("acme/api"));
        assert_eq!(api["source"]["branch"].as_str(), Some("dev"));
        assert_eq!(api["variables"]["A"].as_str(), Some("1"));
        assert_eq!(api["variables"]["B"]["secret"].as_bool(), Some(true));
        assert_eq!(base["services"]["web"]["port"].as_integer(), Some(3000));

        // A source of another kind replaces the base's.
        let overlay: toml::Table =
            toml::from_str("services.api.source = { image = \"nginx\" }").unwrap();
        deep_merge(&mut base, overlay);
        let source = base["services"]["api"]["source"].as_table().unwrap();
        assert_eq!(source.len(), 1);
        assert_eq!(source["image"].as_str(), Some("nginx"));
    }
}
