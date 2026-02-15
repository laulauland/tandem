fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file("schema/tandem.capnp")
        .run()
        .expect("compiling tandem.capnp schema");
}
