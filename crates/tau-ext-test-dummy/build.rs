fn main() {
    tau_client::build::emit_from_root("../..").expect("capture package build identity");
}
