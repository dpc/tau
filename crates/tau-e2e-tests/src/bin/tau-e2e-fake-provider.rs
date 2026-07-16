//! Test-only deterministic provider subprocess.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tau_e2e_tests::fake_provider::run_stdio()
}
