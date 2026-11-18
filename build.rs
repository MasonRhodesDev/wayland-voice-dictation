use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=dist/macos/Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        let template =
            std::fs::read_to_string("dist/macos/Info.plist").expect("dist/macos/Info.plist");
        let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
        let plist = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("Info.plist");
        std::fs::write(&plist, template.replace("@VERSION@", &version))
            .expect("writing Info.plist");
        println!(
            "cargo::rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
