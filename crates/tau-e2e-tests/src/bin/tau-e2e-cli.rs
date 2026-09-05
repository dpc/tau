//! Test-only thin executable for exercising the real Tau CLI command path.

fn main() -> std::process::ExitCode {
    tau_cli::main_with_args()
}
