#![allow(clippy::derive_partial_eq_without_eq)]
pub mod blockscout {
    pub mod fluent_verifier {
        pub mod v1 {
            include!(concat!(
            env!("OUT_DIR"),
            "/blockscout.fluent_verifier.v1.rs"
            ));
        }
    }
}
