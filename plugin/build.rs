use std::{env, fs, path::Path};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo");
    let lock = Path::new(&manifest_dir).join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let contents = fs::read_to_string(&lock).expect("read workspace Cargo.lock for MUI revision");
    let mut revision = None;
    for line in contents.lines().filter(|line| {
        line.starts_with("source = ") && line.contains("github.com/Matari-Audio/MUI")
    }) {
        let sha = line
            .strip_prefix("source = \"git+https://github.com/Matari-Audio/MUI?")
            .and_then(|source| source.strip_suffix('"'))
            .and_then(|source| source.rsplit_once('#'))
            .map(|(_, sha)| sha)
            .filter(|sha| {
                sha.len() == 40
                    && sha
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
            .expect("MUI Cargo.lock source must have a full lowercase Git revision");
        match revision {
            None => revision = Some(sha),
            Some(previous) => assert_eq!(previous, sha, "mixed MUI revisions in Cargo.lock"),
        }
    }
    println!(
        "cargo:rustc-env=RELAY_MUI_REVISION={}",
        revision.expect("MUI Git source missing from Cargo.lock")
    );
}
