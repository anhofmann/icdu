//! icdu is a Unix program: it works with device and inode numbers, block counts and
//! `unlink`, which Windows does not have in this form. A build for anything else would end in
//! a page of errors about what is missing. This stops it with one sentence instead.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_CFG_UNIX").is_none() {
        eprintln!("icdu supports Linux and macOS only.");
        std::process::exit(1);
    }
}
