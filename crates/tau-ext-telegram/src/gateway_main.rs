fn main() -> Result<(), Box<dyn std::error::Error>> {
    tau_ext_telegram::run_gateway_from_env()
}
