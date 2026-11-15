//! The global allocator under test, chosen by a Cargo feature.

#[cfg(all(feature = "alloc-mimalloc", feature = "alloc-jemalloc"))]
compile_error!("choose one allocator feature");

#[cfg(feature = "alloc-mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "alloc-jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// The name of the allocator this binary was built with.
#[must_use]
pub const fn name() -> &'static str {
    if cfg!(feature = "alloc-mimalloc") {
        "mimalloc"
    } else if cfg!(feature = "alloc-jemalloc") {
        "jemalloc"
    } else {
        "system"
    }
}
