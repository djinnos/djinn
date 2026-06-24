#[cfg(test)]
mod env_check {
    #[test]
    fn print_env() {
        eprintln!("TEST_POSTGRES_URL = {:?}", std::env::var("TEST_POSTGRES_URL"));
        eprintln!("DJINN_TEST_DATABASE_URL = {:?}", std::env::var("DJINN_TEST_DATABASE_URL"));
        eprintln!("DATABASE_URL = {:?}", std::env::var("DATABASE_URL"));
    }
}
