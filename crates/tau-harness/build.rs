fn main() {
    tau_client::build::emit_from_root("../..").expect("capture package build identity");
    built::write_built_file().expect("failed to write built file");
}
