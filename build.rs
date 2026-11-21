fn main() {
    cc::Build::new()
        .include("vendor")
        .include("vendor/playfair")
        .files([
            "vendor/fp_shim.c",
            "vendor/playfair/hand_garble.c",
            "vendor/playfair/modified_md5.c",
            "vendor/playfair/omg_hax.c",
            "vendor/playfair/playfair.c",
            "vendor/playfair/sap_hash.c",
        ])
        .warnings(false)
        .compile("playfair");
    println!("cargo:rerun-if-changed=vendor");
}
