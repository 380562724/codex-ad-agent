use toml::Value as TomlValue;

/// Flattens a parsed TOML document into dotted-path `(key, value)` pairs suitable for
/// `cli_overrides: Vec<(String, TomlValue)>`, whose entries are applied via
/// `apply_toml_override` (config/src/overrides.rs), which splits each key on `.`.
pub(crate) fn flatten_toml_table(value: &TomlValue) -> Vec<(String, TomlValue)> {
    let mut entries = Vec::new();
    flatten_into(value, &mut Vec::new(), &mut entries);
    entries
}

fn flatten_into(value: &TomlValue, path: &mut Vec<String>, entries: &mut Vec<(String, TomlValue)>) {
    match value {
        TomlValue::Table(table) => {
            for (key, nested) in table {
                path.push(key.clone());
                flatten_into(nested, path, entries);
                path.pop();
            }
        }
        leaf => entries.push((path.join("."), leaf.clone())),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use toml::Value as TomlValue;

    use super::flatten_toml_table;

    #[test]
    fn flattens_nested_tables_into_dotted_paths() {
        let doc: TomlValue = toml::from_str(
            r#"
            model = "qwen3.8-max"
            model_provider = "cowork"

            [model_providers.cowork]
            name = "Cowork"
            base_url = "https://api.quchenyang.com/llm/v1"
            requires_openai_auth = true
            "#,
        )
        .expect("fixture should parse as TOML");

        let mut flattened = flatten_toml_table(&doc);
        flattened.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            flattened,
            vec![
                (
                    "model".to_string(),
                    TomlValue::String("qwen3.8-max".to_string())
                ),
                (
                    "model_provider".to_string(),
                    TomlValue::String("cowork".to_string())
                ),
                (
                    "model_providers.cowork.base_url".to_string(),
                    TomlValue::String("https://api.quchenyang.com/llm/v1".to_string())
                ),
                (
                    "model_providers.cowork.name".to_string(),
                    TomlValue::String("Cowork".to_string())
                ),
                (
                    "model_providers.cowork.requires_openai_auth".to_string(),
                    TomlValue::Boolean(true)
                ),
            ]
        );
    }

    #[test]
    fn treats_arrays_as_leaf_values_not_nested_paths() {
        let doc: TomlValue =
            toml::from_str("supported_reasoning_levels = []\ntags = [\"a\", \"b\"]")
                .expect("fixture should parse as TOML");

        let mut flattened = flatten_toml_table(&doc);
        flattened.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(
            flattened,
            vec![
                (
                    "supported_reasoning_levels".to_string(),
                    TomlValue::Array(vec![])
                ),
                (
                    "tags".to_string(),
                    TomlValue::Array(vec![
                        TomlValue::String("a".to_string()),
                        TomlValue::String("b".to_string()),
                    ])
                ),
            ]
        );
    }

    #[test]
    fn empty_document_flattens_to_no_entries() {
        let doc: TomlValue = toml::from_str("").expect("empty document should parse");

        assert_eq!(flatten_toml_table(&doc), Vec::new());
    }
}
