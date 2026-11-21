fn main() {
    let d = std::env::args().nth(1).unwrap();
    if std::env::args().nth(2).as_deref() == Some("clicks") {
        fast::clean::microbench_clicks(std::path::Path::new(&d));
    } else {
        fast::clean::microbench(std::path::Path::new(&d));
    }
}
