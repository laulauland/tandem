use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const SCHEMA_FILE: &str = "schema/tandem.capnp";
const CHECKED_IN_BINDINGS: &str = "src/tandem_capnp.rs";
const REGEN_ENV: &str = "TANDEM_REGENERATE_BINDINGS";

fn copy_checked_in_bindings(out_file: &PathBuf) {
    fs::copy(CHECKED_IN_BINDINGS, out_file).unwrap_or_else(|err| {
        panic!(
            "copying checked-in Cap'n Proto bindings from {CHECKED_IN_BINDINGS} to {}: {err}",
            out_file.display()
        )
    });
}

fn has_capnp_binary() -> bool {
    Command::new("capnp")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn compile_schema_to_out_dir() -> Result<(), String> {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .file(SCHEMA_FILE)
        .run()
        .map_err(|err| err.to_string())
}

fn regenerate_checked_in_bindings() -> Result<(), String> {
    capnpc::CompilerCommand::new()
        .src_prefix("schema")
        .output_path("src")
        .file(SCHEMA_FILE)
        .run()
        .map_err(|err| err.to_string())
}

fn main() {
    println!("cargo:rerun-if-changed={SCHEMA_FILE}");
    println!("cargo:rerun-if-changed={CHECKED_IN_BINDINGS}");
    println!("cargo:rerun-if-env-changed={REGEN_ENV}");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set by cargo"));
    let out_file = out_dir.join("tandem_capnp.rs");

    let regenerate = env::var_os(REGEN_ENV).is_some();
    if regenerate {
        if !has_capnp_binary() {
            panic!(
                "{REGEN_ENV} is set, but `capnp` was not found in PATH. Install capnp to regenerate {CHECKED_IN_BINDINGS}."
            );
        }
        regenerate_checked_in_bindings()
            .unwrap_or_else(|err| panic!("regenerating {CHECKED_IN_BINDINGS}: {err}"));
        println!("cargo:warning=regenerated {CHECKED_IN_BINDINGS} from {SCHEMA_FILE}");
    }

    if has_capnp_binary() {
        if let Err(err) = compile_schema_to_out_dir() {
            println!(
                "cargo:warning=capnp found but schema compilation failed ({err}). Falling back to checked-in {CHECKED_IN_BINDINGS}"
            );
            copy_checked_in_bindings(&out_file);
        }
    } else {
        println!(
            "cargo:warning=capnp executable not found. Using checked-in {CHECKED_IN_BINDINGS}"
        );
        copy_checked_in_bindings(&out_file);
    }
}
