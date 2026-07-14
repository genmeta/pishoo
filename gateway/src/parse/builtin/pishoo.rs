use crate::parse::{
    domain::ResolvedConfigPath,
    registry::{
        CascadePolicy, ConfigRegistry, DirectiveSpec, DuplicatePolicy, LocalDirectiveKey,
        ReloadImpact, SingleCardinality, TransportPolicy, TypedDirectiveDefinition, context,
    },
    types::StringList,
};

const PID_DEFINITION: TypedDirectiveDefinition<ResolvedConfigPath, SingleCardinality> =
    TypedDirectiveDefinition::single_leaf(
        context::PISHOO,
        "pid",
        DuplicatePolicy::Reject,
        CascadePolicy::None,
        TransportPolicy::HypervisorOnly,
        ReloadImpact::Supervisor,
    );
const WORKERS_DEFINITION: TypedDirectiveDefinition<StringList, SingleCardinality> =
    TypedDirectiveDefinition::single_leaf(
        context::PISHOO,
        "workers",
        DuplicatePolicy::Reject,
        CascadePolicy::None,
        TransportPolicy::HypervisorOnly,
        ReloadImpact::Supervisor,
    );
const GROUPS_DEFINITION: TypedDirectiveDefinition<StringList, SingleCardinality> =
    TypedDirectiveDefinition::single_leaf(
        context::PISHOO,
        "groups",
        DuplicatePolicy::Reject,
        CascadePolicy::None,
        TransportPolicy::HypervisorOnly,
        ReloadImpact::Supervisor,
    );

pub(crate) const PID_KEY: LocalDirectiveKey<ResolvedConfigPath> = PID_DEFINITION.key();
pub(crate) const WORKERS_KEY: LocalDirectiveKey<StringList> = WORKERS_DEFINITION.key();
pub(crate) const GROUPS_KEY: LocalDirectiveKey<StringList> = GROUPS_DEFINITION.key();

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
            DuplicatePolicy::Append,
            CascadePolicy::None,
            TransportPolicy::WorkerLocalOnly,
            ReloadImpact::Supervisor,
        ),
    );
    PID_DEFINITION.register(registry);
    WORKERS_DEFINITION.register(registry);
    GROUPS_DEFINITION.register(registry);
    registry.register_v1_snapshot_directives();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::parse::{
        domain::ResolvedConfigPath,
        tests::{first_pishoo, parse_doc},
        types::{AccessRulesUri, MimeTypes, StringList},
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
                .require::<ResolvedConfigPath>("pid")
                .expect("pid should be typed")
                .as_ref()
                .as_ref(),
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
                .require::<crate::parse::domain::ResolvedConfigPath>("pid")
                .expect("pid should be typed")
                .as_ref()
                .as_ref(),
            dir.join("run/pishoo.pid")
        );
    }
}
