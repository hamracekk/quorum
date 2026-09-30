//! Migrations are embedded at compile time by `sqlx::migrate!`; rebuild when they change.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
