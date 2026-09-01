use recordings::Options;

fn main() {
    if let Err(error) = recordings::run(Options::parse()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
