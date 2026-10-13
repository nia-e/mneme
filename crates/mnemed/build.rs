//! Ship exactly the installer-owned runtime plus its setup entry point/helper.
use std::{collections::BTreeSet, env, fs, path::PathBuf};
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
        "library/publication_policy.py".to_owned(),
        "library/host_process.py".to_owned(),
    ];
    let mut seen_programs = BTreeSet::new();
    for item in programs.split(',').filter(|item| !item.trim().is_empty()) {
        let name = item
            .trim()
            .strip_prefix('"')
            .and_then(|name| name.strip_suffix('"'))
            .expect("installer PROGRAMS must contain only double-quoted filenames");
        let stem = name.strip_suffix(".py").expect("Python runtime filename");
        assert!(
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "installer PROGRAMS must contain plain Python module filenames"
        );
        assert!(
            seen_programs.insert(name),
            "duplicate installer PROGRAMS entry: {name}"
        );
        files.push(format!("codex/{name}"));
    }
    files.sort();
    files.dedup();
    let mut generated = String::from("const BUNDLE: &[(&str, &[u8])] = &[\n");
    for name in files {
        // Library publication is also a standalone helper: ship its bounded
        // process dependency beside it, from the one maintained codex source.
        let source = if name == "library/host_process.py" {
            "codex/host_process.py"
        } else {
            &name
        };
        let path = root.join("integrations").join(source);
        assert!(
            path.is_file(),
            "missing init bundle file: {}",
            path.display()
        );
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
