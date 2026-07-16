//! Standalone wrapper for Tau's test-only dummy tool extension.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tau_ext_test_dummy::run_stdio()
}
