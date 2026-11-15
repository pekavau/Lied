//! `sqlx::migrate!` embeds `/migrations` at compile time, but Cargo does not
//! know the directory is an input: without this, adding a migration would not
//! rebuild the crate, and the binary would keep the old set (issue #87).
fn main() {
    println!("cargo:rerun-if-changed=../migrations");
}
