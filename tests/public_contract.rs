//! Public API checks for fail-closed provider configuration and redaction.

use ores_mcp_server_core_libs::{
    ai::{ProviderConfiguration, ProviderKind, ProviderRegistry, ProviderState},
    bounds::Limits,
    redaction::Secret,
};

#[test]
fn all_fixed_origin_connectors_can_be_registered_without_network_access() {
    let limits = Limits::default();
    let configuration = ProviderKind::ALL
        .into_iter()
        .try_fold(ProviderConfiguration::new(), |configuration, provider| {
            configuration.with_provider(provider, "integration-test-key", "configured-model")
        })
        .expect("valid configuration");
    let registry = ProviderRegistry::from_configuration(limits, configuration);

    let statuses = registry.statuses();
    assert_eq!(statuses.len(), ProviderKind::ALL.len());
    assert!(
        statuses
            .iter()
            .all(|status| status.state() == ProviderState::Ready)
    );
}

#[test]
fn public_secret_formatting_is_redacted() {
    let secret = Secret::new("must-not-appear").expect("valid secret");
    assert!(!format!("{secret:?}").contains("must-not-appear"));
    assert!(!secret.to_string().contains("must-not-appear"));
}
