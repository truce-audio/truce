fn main() {
    let output = std::process::Command::new("rustc")
        .arg("--version")
        .arg("--verbose")
        .output()
        .expect("rustc not found");
    let version = String::from_utf8_lossy(&output.stdout);
    let hash = fnv1a_64(version.as_bytes());
    println!("cargo:rustc-env=TRUCE_RUSTC_HASH={hash}");
    println!("cargo:rerun-if-changed=../../rust-toolchain.toml");
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
