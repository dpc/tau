fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--workdir") {
        if let Some(dir) = args.get(pos + 1) {
            std::env::set_current_dir(dir)?;
        }
    }
    tau_ext_shell::run_stdio()
}
