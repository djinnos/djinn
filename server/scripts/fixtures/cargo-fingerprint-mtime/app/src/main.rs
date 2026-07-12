fn main() {
    let _ = ordinary::ordinary_value();
    pm::make_greeting!();
    let mark = env!("BUILD_MARK");
    println!("{} {}", ordinary::ordinary_value(), mark);
}
