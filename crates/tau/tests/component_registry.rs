use std::collections::BTreeSet;
use std::process::Command;

/// Guards the production `tau` wrapper's component registry against drifting
/// away from the harness built-in extension suffixes that launch
/// `tau component <name>` children. A missing registration would make the
/// corresponding built-in unusable even though the harness configuration still
/// resolves it successfully.
#[test]
fn bundled_extension_components_are_discoverable() {
    let tau_bin = std::env::var("CARGO_BIN_EXE_tau").expect("CARGO_BIN_EXE_tau");
    let output = Command::new(tau_bin)
        .arg("component")
        .arg("__missing_component_for_discovery_test__")
        .output()
        .expect("run tau component");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf8");
    assert!(stderr.contains("unknown component"));

    for component in expected_builtin_component_names() {
        assert!(
            stderr.contains(&component),
            "unknown-component diagnostics should list bundled component {component}; stderr:\n{stderr}"
        );
    }
}

fn expected_builtin_component_names() -> BTreeSet<String> {
    tau_harness::builtin_extensions()
        .into_iter()
        .filter_map(|extension| match extension.suffix.as_slice() {
            [component, name] if component == "component" => Some(name.clone()),
            _ => None,
        })
        .collect()
}
