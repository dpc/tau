use tau_config::settings as path_tau_config_settings;

use super::*;

/// The CLI fallback role must stay aligned with the built-in harness default so
/// headless creation does not silently select a different role.
#[test]
fn fallback_role_matches_built_in_harness_default() {
    let built_in = path_tau_config_settings::HarnessSettings::built_in();
    assert!(built_in.roles.contains_key(DEFAULT_AGENT_ROLE));
}
