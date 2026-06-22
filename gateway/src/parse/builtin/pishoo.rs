use crate::parse::{
    registry::{ConfigRegistry, DirectiveSpec, MergePolicy, context},
    types::{
        AccessRulesUri, BoolConfig, DefaultType, GzipCompLevel, GzipMinLength, MimeTypes,
        PathConfig, StringList,
    },
};

pub fn register(registry: &mut ConfigRegistry) {
    registry.register_context(crate::parse::registry::ContextSpec {
        key: context::PISHOO,
        finalize: None,
    });
    registry.register_directive(
        context::ROOT,
        DirectiveSpec::context_empty(
            "pishoo",
            vec![context::ROOT],
            context::PISHOO,
            MergePolicy::Append,
        ),
    );
    register_leaf::<PathConfig>(registry, "pid");
    register_leaf::<StringList>(registry, "workers");
    register_leaf::<StringList>(registry, "groups");
    register_leaf::<AccessRulesUri>(registry, "access_rules");
    register_leaf::<BoolConfig>(registry, "gzip");
    register_leaf::<BoolConfig>(registry, "gzip_vary");
    register_leaf::<GzipMinLength>(registry, "gzip_min_length");
    register_leaf::<GzipCompLevel>(registry, "gzip_comp_level");
    register_leaf::<StringList>(registry, "gzip_types");
    register_leaf::<DefaultType>(registry, "default_type");
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec::raw_value::<MimeTypes>(
            "types",
            vec![context::PISHOO],
            MergePolicy::RejectDuplicate,
        ),
    );
}

fn register_leaf<T>(registry: &mut ConfigRegistry, name: &'static str)
where
    T: crate::parse::registry::DirectiveValue,
    for<'input, 'directive> T: TryFrom<
            &'input crate::parse::registry::DirectiveInput<'directive>,
            Error = <T as crate::parse::registry::DirectiveValue>::Error,
        >,
{
    registry.register_directive(
        context::PISHOO,
        DirectiveSpec::leaf_value::<T>(name, vec![context::PISHOO], MergePolicy::RejectDuplicate),
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::parse::{
        tests::{first_pishoo, parse_doc},
        types::{AccessRulesUri, MimeTypes, PathConfig, StringList},
    };

    #[test]
    fn parse_pishoo_groups_collects_all_values() {
        let pishoo = first_pishoo(&parse_doc("pishoo { groups admin viewer api; }"));

        let groups = pishoo
            .require::<StringList>("groups")
            .expect("groups should be typed");
        assert_eq!(groups.0, vec!["admin", "viewer", "api"]);
    }

    #[test]
    fn parse_pishoo_gzip_types_collects_extensions() {
        let pishoo = first_pishoo(&parse_doc("pishoo { gzip_types txt css js; }"));

        let gzip_types = pishoo
            .require::<StringList>("gzip_types")
            .expect("gzip_types should be typed");
        assert_eq!(gzip_types.0, vec!["txt", "css", "js"]);
    }

    #[test]
    fn parse_pishoo_types_block_parses_mime_types() {
        let pishoo = first_pishoo(&parse_doc(
            "pishoo { types { text/plain txt; application/json json; } }",
        ));

        let types = pishoo
            .require::<MimeTypes>("types")
            .expect("types should be typed")
            .0
            .clone();

        assert_eq!(types.get("txt").unwrap().to_str().unwrap(), "text/plain");
        assert_eq!(
            types.get("json").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn parse_string_list_directive_collects_all_values() {
        let conf = "pishoo { workers admin viewer api; }";
        let pishoo = first_pishoo(&parse_doc(conf));

        let workers = pishoo
            .require::<StringList>("workers")
            .expect("workers should be typed");
        assert_eq!(workers.0, vec!["admin", "viewer", "api"]);
    }

    #[test]
    fn parse_pid_and_access_rules_keep_domain_types() {
        let pishoo = first_pishoo(&parse_doc(
            "pishoo { pid /tmp/pishoo-test.pid; access_rules sqlite:///tmp/rules.db?mode=ro; }",
        ));

        assert_eq!(
            pishoo
                .require::<PathConfig>("pid")
                .expect("pid should be typed")
                .0,
            PathBuf::from("/tmp/pishoo-test.pid")
        );
        assert_eq!(
            pishoo
                .require::<AccessRulesUri>("access_rules")
                .expect("access_rules should be typed")
                .0
                .as_str(),
            "sqlite:///tmp/rules.db?mode=ro"
        );
    }

    #[tokio::test]
    async fn parse_relative_pid_uses_root_config_dir() {
        let dir = std::env::temp_dir().join(format!(
            "gateway-relative-pid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        ));
        std::fs::create_dir_all(dir.join("run")).expect("create run dir");
        std::fs::write(dir.join("pishoo.conf"), "pishoo { pid ./run/pishoo.pid; }")
            .expect("write config");

        let registry = crate::parse::default_registry();
        let parsed = crate::parse::load_config_file(
            &dir.join("pishoo.conf"),
            &registry,
            crate::parse::registry::BuildOptions::default(),
        )
        .await
        .expect("config should load");

        let pishoo = crate::parse::tests::first_pishoo(&parsed);
        assert_eq!(
            pishoo
                .require::<crate::parse::types::PathConfig>("pid")
                .expect("pid should be typed")
                .0,
            dir.join("run/pishoo.pid")
        );
    }
}
