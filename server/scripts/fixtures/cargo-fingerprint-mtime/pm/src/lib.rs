use proc_macro::TokenStream;

#[proc_macro]
pub fn make_greeting(_input: TokenStream) -> TokenStream {
    "fn _greeting() -> &'static str { \"hello\" }".parse().unwrap()
}
