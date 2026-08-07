use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=macos/Info.plist");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let plist = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("missing manifest dir"))
        .join("macos/Info.plist");
    println!(
        "cargo::rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
