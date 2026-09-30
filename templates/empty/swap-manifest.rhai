// Post-generation hook, see cargo-generate.toml.
file::delete("Cargo.toml");
file::rename("Cargo.generated.toml", "Cargo.toml");
file::delete("swap-manifest.rhai");
