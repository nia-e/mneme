//! Ship exactly the installer-owned runtime plus its setup entry point/helper.
use std::{env, fs, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let installer = root.join("integrations/codex/install.py");
    println!("cargo:rerun-if-changed={}", installer.display());
    let source = fs::read_to_string(&installer).unwrap();
    // PROGRAMS is deliberately a plain literal list, shared with the installer.
    // Fail the build if that contract changes; don't execute Python at build time.
    let programs = source
        .split_once("PROGRAMS = (")
        .expect("installer PROGRAMS list")
        .1
        .split_once(')')
        .expect("closed installer PROGRAMS list")
        .0;
    let mut files = vec![
        "codex/install.py".to_owned(),
        "codex/project_setup.py".to_owned(),
        "codex/hook_trust.py".to_owned(),
        "library/library.py".to_owned(),
    ];
    for item in programs.split(',').filter(|item| !item.trim().is_empty()) {
        let name = item
            .trim()
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .expect("installer PROGRAMS must contain only double-quoted filenames");
        assert!(name.ends_with(".py") && !name.contains('/') && !name.contains('\\'));
        files.push(format!("codex/{name}"));
    }
    files.sort();
    files.dedup();
    let mut generated = String::from("const BUNDLE: &[(&str, &[u8])] = &[\n");
    for name in files {
        let path = root.join("integrations").join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        generated.push_str(&format!(
            "({name:?}, include_bytes!({:?})),\n",
            path.to_str().unwrap()
        ));
    }
    generated.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("init_bundle.rs"),
        generated,
    )
    .unwrap();
}
