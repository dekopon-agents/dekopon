use dekopon_config::LocalCatalog;
use dekopon_testkit::{AgentBuilder, CapabilityBuilder, ProviderBuilder, temporary_config};

#[test]
fn builders_produce_a_catalog_accepted_by_config_loader() {
    let provider_id = "github".parse().expect("valid fixture provider ID");
    let capability_id = "github.pull-request.read"
        .parse()
        .expect("valid fixture capability ID");
    let resources = [
        serde_yaml::to_string(&ProviderBuilder::new("github").build())
            .expect("provider serializes"),
        serde_yaml::to_string(
            &CapabilityBuilder::new("github.pull-request.read", provider_id).build(),
        )
        .expect("capability serializes"),
        serde_yaml::to_string(
            &AgentBuilder::new("reviewer")
                .capability(capability_id)
                .provider("github".parse().expect("valid fixture provider ID"))
                .build(),
        )
        .expect("agent serializes"),
    ];
    let file = temporary_config(&resources.join("---\n")).expect("temporary configuration");

    let catalog = LocalCatalog::load(file.path()).expect("builders produce valid resources");

    assert_eq!(catalog.agents().len(), 1);
    assert_eq!(catalog.capabilities().len(), 1);
    assert_eq!(catalog.providers().len(), 1);
}
