//! Rebuild when migrations change: they are embedded with `sqlx::migrate!`.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
