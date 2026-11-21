#[global_allocator]
static A: fast::alloc::HugeAlloc = fast::alloc::HugeAlloc;
fn main() {
    fast::clean::microbench(std::path::Path::new(&std::env::args().nth(1).unwrap()));
}
