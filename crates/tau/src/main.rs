fn main() -> std::process::ExitCode {
    tau_cli::main_with_args_and_components(&[
        tau_cli::Component {
            name: "ext-shell",
            runner: tau_ext_shell::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-test-dummy",
            runner: tau_ext_test_dummy::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-provider-builtin",
            runner: tau_ext_provider_builtin::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-rhai",
            runner: tau_ext_rhai::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-std-notifications",
            runner: tau_ext_std_notifications::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-utils",
            runner: tau_ext_utils::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
        tau_cli::Component {
            name: "ext-websearch",
            runner: tau_ext_websearch::run_stdio,
            logging: tau_cli::ComponentLogging::RunnerManaged,
        },
    ])
}
